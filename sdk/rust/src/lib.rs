//! Write a [nanovm](https://github.com/carsonfarmer/nanovm) app: one async
//! function from a [`Request`] to a [`Response`]. nanovm runs it as durable
//! objects: the first segment of a URL, `/{object}/…`, names an object, and
//! each object is one live instance serving one request at a time, with its
//! own persistent volume ([`data`]), key-value store ([`kv`]), SQLite
//! database ([`sqlite`]) and WebSockets ([`ws`]). Its memory is lost when
//! it is evicted; its volume is not.
//!
//! ```no_run
//! use nanovm_sdk::{Request, Response, Result, kv};
//!
//! nanovm_sdk::serve!(handle);
//!
//! async fn handle(req: Request) -> Result {
//!     match (req.method(), req.path()) {
//!         ("POST", "/incr") => Ok(Response::text(kv::increment("n", 1).await?.to_string())),
//!         ("GET", "/") => Ok(Response::text(kv::get_text("n").await?.unwrap_or("0".into()))),
//!         _ => Ok(Response::not_found()),
//!     }
//! }
//! ```
//!
//! Build it with `tools/build-app.sh` (cargo, then `wac plug` of the
//! key-value and SQLite providers the app uses) and serve it with
//! `nanovm run app.wasm`.
//!
//! **Commits.** What a request changes in `/data`, the key-value store and
//! the database, and what it publishes to WebSockets, takes effect together
//! and durably when it answers, whatever the status. Returning an [`Error`]
//! (as `?` does) or panicking rolls all of it back, so a failed step never
//! leaves half a change behind. Answer with a 4xx or 5xx only when what was done so far
//! should stand.

use std::fmt;
use std::future::Future;

pub use wasip3;
use wasip3::http::types as wit;
use wasip3::{wit_future, wit_stream};
use wit_bindgen::rt::async_support::FutureReader;

pub mod config;
pub mod fs;
pub mod http;
pub mod kv;
pub mod sqlite;
pub mod ws;

pub use fs::{data, tmp};
pub use http::{fetch, object};
/// With the `json` feature: `serde_json::json!` builds bodies, and
/// [`Request::json`] reads them.
#[cfg(feature = "json")]
pub use serde_json;

/// Exports `handler`, an `async fn(Request) -> Result`, as the component's
/// `wasi:http` handler.
#[macro_export]
macro_rules! serve {
    ($handler:path) => {
        struct __NanovmApp;
        impl $crate::wasip3::exports::http::handler::Guest for __NanovmApp {
            async fn handle(
                request: $crate::wasip3::http::types::Request,
            ) -> ::core::result::Result<$crate::wasip3::http::types::Response, $crate::wasip3::http::types::ErrorCode>
            {
                $crate::__serve(request, $handler).await
            }
        }
        $crate::wasip3::http::service::export!(__NanovmApp);
    };
}

#[doc(hidden)]
pub async fn __serve<F: Future<Output = Result>>(
    request: wit::Request,
    handler: impl FnOnce(Request) -> F,
) -> core::result::Result<wit::Response, wit::ErrorCode> {
    AFTER.set(Some(Vec::new()));
    // A failed request's instance is discarded: nothing runs after it.
    let response = handler(Request::read(request).await).await.map_err(|Error(why)| wit::ErrorCode::InternalError(Some(why)))?;
    let streamed = response.writer.is_some();
    let (response, out) = response.answer()?;
    match streamed || AFTER.with_borrow(|a| a.as_ref().is_some_and(|a| !a.is_empty())) {
        // Started only once the answer is out, so that no panic in it can
        // fail the answer (one before the node has taken it would).
        true => wasip3::spawn_local(async move {
            let _ = out.await;
            AFTER.take().into_iter().flatten().for_each(wasip3::spawn_local);
        }),
        false => AFTER.set(None),
    }
    Ok(response)
}

thread_local! {
    /// Work for after the answer, waiting until it is out; `None` once it is.
    static AFTER: std::cell::RefCell<Option<Vec<std::pin::Pin<Box<dyn Future<Output = ()>>>>>> = const { std::cell::RefCell::new(None) };
}

/// Runs `work` after the answer, once the client has all of it (or has
/// gone): the client doesn't wait for it (a webhook's sender, say), and
/// nothing `work` does can change or fail the answer. The object takes no
/// other request until `work`, and any work it passes to `after` in turn,
/// is done, within the app's `post_return_timeout_ms`. What all of it
/// changes commits together when it is done; if any of it panics or time
/// runs out, all of that rolls back and the instance is replaced, while the
/// answer and what the handler changed stand. Nothing runs if the handler
/// returns an error.
pub fn after(work: impl Future<Output = ()> + 'static) {
    let work = Box::pin(work);
    AFTER.with_borrow_mut(|a| match a {
        Some(waiting) => waiting.push(work),
        None => wasip3::spawn_local(work),
    });
}

/// Why a request failed. A handler that returns one rolls back everything the
/// request changed; the client gets a `500` (`x-nanovm-error: guest-error`)
/// with the message. Any `std::error::Error` converts into one, so `?` works
/// on file, key-value and HTTP errors alike.
pub struct Error(String);

impl Error {
    pub fn new(message: impl Into<String>) -> Error {
        Error(message.into())
    }
}

impl<E: std::error::Error> From<E> for Error {
    fn from(e: E) -> Error {
        Error(e.to_string())
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A handler's result: a [`Response`], unless the request failed.
pub type Result<T = Response, E = Error> = core::result::Result<T, E>;

/// This object's id: the `{object}` of `/{object}/…`.
pub fn id() -> String {
    env("NANOVM_OBJECT_ID")
}

/// This app's name on the node.
pub fn app() -> String {
    env("NANOVM_APP")
}

/// Environment variable `key` (the `env` table of the app's `nanovm.json`), or "".
pub fn env(key: &str) -> String {
    std::env::var(key).unwrap_or_default()
}

/// `s` as a JSON string, quoted and escaped: JSON without the `json` feature.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Waits without using CPU.
pub async fn sleep(duration: std::time::Duration) {
    wasip3::clocks::monotonic_clock::wait_for(duration.as_nanos() as u64).await
}

/// A request, read in full (the node bounds its body by the app's
/// `max_request_body_kb`).
pub struct Request {
    method: String,
    url: String,
    path: String,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    pub async fn read(request: wit::Request) -> Request {
        let method = match request.get_method() {
            wit::Method::Get => "GET".into(),
            wit::Method::Head => "HEAD".into(),
            wit::Method::Post => "POST".into(),
            wit::Method::Put => "PUT".into(),
            wit::Method::Delete => "DELETE".into(),
            wit::Method::Patch => "PATCH".into(),
            wit::Method::Options => "OPTIONS".into(),
            wit::Method::Connect => "CONNECT".into(),
            wit::Method::Trace => "TRACE".into(),
            wit::Method::Other(m) => m,
        };
        let headers = request.get_headers().copy_all().into_iter();
        let headers = headers.map(|(k, v)| (k.to_ascii_lowercase(), String::from_utf8_lossy(&v).into_owned())).collect();
        let scheme = match request.get_scheme() {
            Some(wit::Scheme::Https) => "https",
            _ => "http",
        };
        let authority = request.get_authority().unwrap_or_else(|| "localhost".into());
        let pq = request.get_path_with_query().unwrap_or_else(|| "/".into());
        let url = format!("{scheme}://{authority}{pq}");
        let (path, query) = pq.split_once('?').unwrap_or((&pq, ""));
        let pair = |kv: &str| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (percent_decode(k), percent_decode(v))
        };
        let query = query.split('&').filter(|kv| !kv.is_empty()).map(pair).collect();
        let path = path.to_string();
        let (result_tx, result_rx) = wit_future::new(|| Ok(()));
        let (body, _trailers) = wit::Request::consume_body(request, result_rx);
        let body = body.collect().await;
        drop(result_tx);
        Request { method, url, path, query, headers, body }
    }

    /// `GET`, `POST`, ….
    pub fn method(&self) -> &str {
        &self.method
    }

    /// The whole URL, as received: `http://{host}/a/b?q=1` for `/{object}/a/b?q=1`.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The path within the object: `/{object}/a/b` arrives as `/a/b`.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Query parameter `key`, percent-decoded; the last one if repeated.
    pub fn query(&self, key: &str) -> Option<&str> {
        self.query.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// Query parameter `key` as a `T`, or `default` if it is missing or isn't one.
    pub fn query_or<T: std::str::FromStr>(&self, key: &str, default: T) -> T {
        self.query(key).and_then(|v| v.parse().ok()).unwrap_or(default)
    }

    /// Header `name`, in any case; the last one if repeated.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().rev().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// Every header, names in lower case.
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// Cookie `name`, as the client sent it; set one with
    /// `Response::header("set-cookie", "name=value; Path=/; HttpOnly")`.
    pub fn cookie(&self, name: &str) -> Option<&str> {
        let cookies = self.headers.iter().filter(|(k, _)| k == "cookie").flat_map(|(_, v)| v.split(';'));
        cookies.filter_map(|c| c.trim().split_once('=')).find(|(k, _)| *k == name).map(|(_, v)| v.trim_matches('"'))
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub fn into_body(self) -> Vec<u8> {
        self.body
    }

    /// The body as UTF-8.
    pub fn text(&self) -> Result<&str> {
        Ok(std::str::from_utf8(&self.body)?)
    }

    /// The body as JSON, with the `json` feature.
    #[cfg(feature = "json")]
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        Ok(serde_json::from_slice(&self.body)?)
    }
}

fn percent_decode(s: &str) -> String {
    let (bytes, mut out, mut i) = (s.as_bytes(), Vec::with_capacity(s.len()), 0);
    while i < bytes.len() {
        let hex = bytes.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok());
        match (bytes[i], hex) {
            (b'+', _) => out.push(b' '),
            (b'%', Some(b)) => {
                out.push(b);
                i += 2;
            }
            (b, _) => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A response, built up and returned from a handler:
/// `Response::json(body).status(201).header("location", url)`.
#[derive(Debug, Clone)]
pub struct Response {
    status: u16,
    headers: Vec<(String, Vec<u8>)>,
    body: Vec<u8>,
    writer: Option<Writer>,
}

/// What writes a streamed body ([`Response::stream`]); taken once.
#[derive(Clone)]
#[allow(clippy::type_complexity)]
struct Writer(std::rc::Rc<std::cell::Cell<Option<Box<dyn FnOnce(Body) -> std::pin::Pin<Box<dyn Future<Output = Result<()>>>>>>>>);

impl fmt::Debug for Writer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Writer")
    }
}

/// A streamed response's body, written as the client reads it.
pub struct Body(wit_bindgen::rt::async_support::StreamWriter<u8>);

impl Body {
    /// Sends `data` on its way; `false` once the client has gone.
    pub async fn write(&mut self, data: impl Into<Vec<u8>>) -> bool {
        self.0.write_all(data.into()).await.is_empty()
    }
}

/// One Server-Sent Event, `data` split into `data:` lines, for
/// [`Response::stream`] with content type `text/event-stream`.
pub fn sse(id: Option<&str>, event: Option<&str>, data: &str) -> String {
    let mut out = String::new();
    for (field, value) in [("id", id), ("event", event)] {
        if let Some(value) = value {
            out.push_str(&format!("{field}: {}\n", value.replace(['\r', '\n'], "")));
        }
    }
    for line in data.split('\n') {
        out.push_str(&format!("data: {}\n", line.strip_suffix('\r').unwrap_or(line)));
    }
    out.push('\n');
    out
}

impl Response {
    /// An empty response.
    pub fn new(status: u16) -> Response {
        Response { status, headers: vec![], body: vec![], writer: None }
    }

    /// `200` with `content_type`, its body written by `body` as the client
    /// reads it, after the handler has returned. Meanwhile the object serves
    /// nothing else: its other requests wait until `body` is done, and it has
    /// the app's `post_return_timeout_ms` for it, after which the object is
    /// stopped and what it changed since answering rolls back. `body` starts
    /// as the handler returns: what it changes before the node has taken the
    /// answer (as it takes the first piece) commits with the answer, the
    /// rest once it ends. If `body` returns an error, the client sees the
    /// body break off, not end, and what was changed since answering rolls
    /// back, as when it panics. Set `content-length` if you know it; without
    /// one the body is chunked.
    pub fn stream<F: Future<Output = Result<()>> + 'static>(content_type: &str, body: impl FnOnce(Body) -> F + 'static) -> Response {
        let writer: Box<dyn FnOnce(Body) -> std::pin::Pin<Box<dyn Future<Output = Result<()>>>>> = Box::new(|b| Box::pin(body(b)));
        let mut response = Response::new(200).header("content-type", content_type);
        response.writer = Some(Writer(std::rc::Rc::new(std::cell::Cell::new(Some(writer)))));
        response
    }

    /// `200`, `text/plain`.
    pub fn text(body: impl Into<String>) -> Response {
        Response::bytes("text/plain; charset=utf-8", body.into())
    }

    /// `200`, `text/html`.
    pub fn html(body: impl Into<String>) -> Response {
        Response::bytes("text/html; charset=utf-8", body.into())
    }

    /// `200`, `application/json`; build the body with `format!` and
    /// [`quote`], or `serde_json` (the `json` feature).
    pub fn json(body: impl Into<String>) -> Response {
        Response::bytes("application/json", body.into())
    }

    /// `200` with `body` of type `content_type`.
    pub fn bytes(content_type: &str, body: impl Into<Vec<u8>>) -> Response {
        Response::new(200).header("content-type", content_type).body(body)
    }

    /// `404`, `not found`.
    pub fn not_found() -> Response {
        Response::text("not found").status(404)
    }

    pub fn status(mut self, status: u16) -> Response {
        self.status = status;
        self
    }

    pub fn header(mut self, name: &str, value: impl AsRef<[u8]>) -> Response {
        self.headers.push((name.into(), value.as_ref().into()));
        self
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Response {
        self.body = body.into();
        self
    }

    /// The response as `wasip3`'s, for a handler written against it directly.
    pub fn into_wit(self) -> core::result::Result<wit::Response, wit::ErrorCode> {
        self.answer().map(|(response, _)| response)
    }

    /// The response as `wasip3`'s, and the future that resolves once it is out.
    fn answer(mut self) -> core::result::Result<(wit::Response, FutureReader<core::result::Result<(), wit::ErrorCode>>), wit::ErrorCode> {
        let (trailers, end) = wit_future::new(|| Ok(None));
        let body = match self.writer.and_then(|w| w.0.take()) {
            Some(writer) => {
                let (tx, rx) = wit_stream::new();
                wasip3::spawn_local(async move {
                    // Its body is closed by now; then come its trailers, or
                    // (on `Ok`, dropped) its end.
                    if let Err(Error(why)) = writer(Body(tx)).await {
                        let _ = trailers.write(Err(wit::ErrorCode::InternalError(Some(why.clone())))).await;
                        after(async move { panic!("the response body failed: {why}") });
                    }
                });
                rx
            }
            None => {
                self.headers.retain(|(k, _)| !k.eq_ignore_ascii_case("content-length"));
                self.headers.push(("content-length".into(), self.body.len().to_string().into()));
                stream(self.body)
            }
        };
        let headers = fields(&self.headers)?;
        let (response, out) = wit::Response::new(headers, Some(body), end);
        let _ = response.set_status_code(self.status);
        Ok((response, out))
    }
}

fn fields(list: &[(String, Vec<u8>)]) -> core::result::Result<wit::Fields, wit::ErrorCode> {
    wit::Fields::from_list(list).map_err(|e| wit::ErrorCode::InternalError(Some(format!("{e:?}"))))
}

/// A stream of `data`, written by a task of its own: the host reads a body
/// only after it has the request or response, so writing first would wait
/// forever.
fn stream(data: Vec<u8>) -> wit_bindgen::rt::async_support::StreamReader<u8> {
    let (mut tx, rx) = wit_stream::new();
    wasip3::spawn_local(async move {
        let _ = tx.write_all(data).await;
    });
    rx
}

type Trailers = core::result::Result<Option<wit::Fields>, wit::ErrorCode>;

fn no_trailers() -> FutureReader<Trailers> {
    wit_future::new(|| Ok(None)).1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(headers: &[(&str, &str)]) -> Request {
        let headers = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Request { method: "GET".into(), url: "http://x/".into(), path: "/".into(), query: vec![], headers, body: vec![] }
    }

    #[test]
    fn server_sent_events() {
        assert_eq!(sse(Some("7"), Some("tick"), "a"), "id: 7\nevent: tick\ndata: a\n\n");
        assert_eq!(sse(None, None, "two\r\nlines"), "data: two\ndata: lines\n\n", "a line per data line");
        assert_eq!(sse(Some("1\n2"), None, ""), "id: 12\ndata: \n\n", "fields can't end the event early");
    }

    #[test]
    fn cookies() {
        let req = request(&[("cookie", "a=1; voter=x7=; b"), ("cookie", r#"c="quoted""#)]);
        assert_eq!(req.cookie("a"), Some("1"));
        assert_eq!(req.cookie("voter"), Some("x7="), "values may hold `=`");
        assert_eq!(req.cookie("c"), Some("quoted"));
        assert_eq!((req.cookie("b"), req.cookie("A"), req.cookie("d")), (None, None, None));
        assert_eq!(request(&[]).cookie("a"), None);
    }
}
