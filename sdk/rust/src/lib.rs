//! Write a [nanovm](https://github.com/carsonfarmer/nanovm) app: one async
//! function from an [`http::Request`] to an [`http::Response`] (the `http`
//! crate's, re-exported here), whose bodies are a [`Body`]. nanovm runs it
//! as durable objects: the first segment of a URL, `/{object}/…`, names an
//! object, and each object is one live instance serving one request at a
//! time, with its own persistent volume ([`data`]), key-value store
//! ([`kv`]), SQLite database ([`sqlite`]) and WebSockets ([`ws`]). Its
//! memory is lost when it is evicted; its volume is not.
//!
//! ```no_run
//! use nanovm_sdk::http::{Request, Response, StatusCode};
//! use nanovm_sdk::{Body, Result, kv};
//!
//! nanovm_sdk::serve!(handle);
//!
//! async fn handle(req: Request<Body>) -> Result {
//!     Ok(match (req.method().as_str(), req.uri().path()) {
//!         ("POST", "/incr") => Response::new(kv::increment("n", 1).await?.to_string().into()),
//!         ("GET", "/") => Response::new(kv::get_text("n").await?.unwrap_or("0".into()).into()),
//!         _ => Response::builder().status(StatusCode::NOT_FOUND).body(Body::empty())?,
//!     })
//! }
//! ```
//!
//! A request arrives read in full: its [`Body`] is the bytes, with
//! [`text`](Body::text) and [`json`](Body::json). [`RequestExt`] adds what
//! `http` leaves out: query parameters and cookies. The path is the one
//! within the object: `/{object}/a/b` arrives as `/a/b`.
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
use std::pin::Pin;

pub use http;
pub use wasip3;
use wasip3::http::types as wit;
use wasip3::{wit_future, wit_stream};
use wit_bindgen::rt::async_support::{FutureReader, StreamReader, StreamWriter};

pub mod config;
mod fetch;
pub mod fs;
pub mod kv;
pub mod sqlite;
pub mod ws;

pub use fetch::{Object, Streaming, fetch, fetch_streaming, object};
pub use fs::{data, tmp};
/// With the `json` feature: `serde_json::json!` builds bodies, and
/// [`Body::json`] reads them.
#[cfg(feature = "json")]
pub use serde_json;

/// Exports `handler`, an `async fn(http::Request<Body>) -> Result`, as the
/// component's `wasi:http` handler.
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
    handler: impl FnOnce(http::Request<Body>) -> F,
) -> core::result::Result<wit::Response, wit::ErrorCode> {
    let result = handler(read(request).await?).await;
    let later = AFTER.with(|a| a.take());
    match result {
        Ok(response) => {
            for work in later {
                wasip3::spawn_local(work);
            }
            let (mut parts, body) = response.into_parts();
            let (body, trailers) = body.into_wit(&mut parts.headers);
            let (response, _) = wit::Response::new(fields(parts.headers)?, Some(body), trailers);
            let _ = response.set_status_code(parts.status.as_u16());
            Ok(response)
        }
        // A failed request's instance is discarded: nothing runs after it.
        Err(Error(why)) => Err(wit::ErrorCode::InternalError(Some(why))),
    }
}

/// The request as `http`'s, its body read in full (the node bounds it by
/// the app's `max_request_body_kb`).
async fn read(request: wit::Request) -> core::result::Result<http::Request<Body>, wit::ErrorCode> {
    let (parts, mut body) = wasip3::http_compat::http_from_wasi_request(request)?.into_parts();
    let request = body.take_unstarted().expect("a new body is unread");
    let (done, result) = wit_future::new(|| Ok(()));
    let (body, _trailers) = wit::Request::consume_body(request, result);
    let body = body.collect().await;
    drop(done);
    Ok(http::Request::from_parts(parts, Body::from(body)))
}

thread_local! {
    static AFTER: std::cell::RefCell<Vec<Pin<Box<dyn Future<Output = ()>>>>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Runs `work` once the handler has answered: the client gets its response
/// without waiting for it (a webhook's sender, say), and the object takes no
/// other request until `work` is done, within the app's
/// `post_return_timeout_ms`. What `work` changes commits when it finishes,
/// and rolls back if it panics or runs out of time (the answer already sent
/// stands). Nothing runs if the handler returns an error.
pub fn after(work: impl Future<Output = ()> + 'static) {
    AFTER.with(|a| a.borrow_mut().push(Box::pin(work)));
}

/// Why a request failed. A handler that returns one rolls back everything the
/// request changed; the client gets a `500` (`x-nanovm-error: guest-error`)
/// with the message. Any `std::error::Error` converts into one, so `?` works
/// on file, key-value, HTTP and `http` builder errors alike.
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

/// A handler's result: an [`http::Response`], unless the request failed.
pub type Result<T = http::Response<Body>, E = Error> = core::result::Result<T, E>;

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

/// What `http` leaves out of a request: its query parameters and cookies.
pub trait RequestExt {
    /// Query parameter `key`, percent-decoded; the last one if repeated.
    fn query(&self, key: &str) -> Option<String>;

    /// Query parameter `key` as a `T`, or `default` if it is missing or isn't one.
    fn query_or<T: std::str::FromStr>(&self, key: &str, default: T) -> T {
        self.query(key).and_then(|v| v.parse().ok()).unwrap_or(default)
    }

    /// Cookie `name`, as the client sent it; set one with a `set-cookie`
    /// header, `name=value; Path=/; HttpOnly`.
    fn cookie(&self, name: &str) -> Option<&str>;
}

impl<B> RequestExt for http::Request<B> {
    fn query(&self, key: &str) -> Option<String> {
        let query = form_urlencoded::parse(self.uri().query()?.as_bytes());
        query.filter(|(k, _)| k == key).last().map(|(_, v)| v.into_owned())
    }

    fn cookie(&self, name: &str) -> Option<&str> {
        let headers = self.headers().get_all(http::header::COOKIE).into_iter().filter_map(|v| v.to_str().ok());
        let mut cookies = headers.flat_map(|v| v.split(';')).filter_map(|c| c.trim().split_once('='));
        cookies.find(|(k, _)| *k == name).map(|(_, v)| v.trim_matches('"'))
    }
}

/// A request's or response's body: bytes, or a response's [stream](Body::stream).
/// Make one from text or bytes with `.into()`. A request's is its bytes,
/// read in full.
pub struct Body(Inner);

type Write = Box<dyn FnOnce(BodyWriter) -> Pin<Box<dyn Future<Output = Result<()>>>>>;

enum Inner {
    Bytes(Vec<u8>),
    Stream(Write),
}

impl Body {
    pub fn empty() -> Body {
        Body(Inner::Bytes(vec![]))
    }

    /// A body written by `write` as the client reads it, after the handler
    /// has returned. Meanwhile the object serves nothing else: its other
    /// requests wait until `write` is done, and it has the app's
    /// `post_return_timeout_ms` for it, after which the object is stopped
    /// and what it changed since answering rolls back. What it changed
    /// before answering has already committed. Set `content-length` if you
    /// know it; without one the body is chunked.
    ///
    /// `write` returning an error breaks the body off: the client sees it
    /// end early (a chunked body without its last chunk), never a short
    /// body that looks whole.
    pub fn stream<F: Future<Output = Result<()>> + 'static>(write: impl FnOnce(BodyWriter) -> F + 'static) -> Body {
        Body(Inner::Stream(Box::new(|w| Box::pin(write(w)))))
    }

    /// The bytes; none for a stream, which is written later.
    pub fn as_bytes(&self) -> &[u8] {
        match &self.0 {
            Inner::Bytes(b) => b,
            Inner::Stream(_) => &[],
        }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        match self.0 {
            Inner::Bytes(b) => b,
            Inner::Stream(_) => vec![],
        }
    }

    /// The body as UTF-8.
    pub fn text(&self) -> Result<&str> {
        Ok(std::str::from_utf8(self.as_bytes())?)
    }

    /// The body as JSON, with the `json` feature.
    #[cfg(feature = "json")]
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        Ok(serde_json::from_slice(self.as_bytes())?)
    }

    fn is_stream(&self) -> bool {
        matches!(self.0, Inner::Stream(_))
    }

    /// The body as `wasi:http` streams it, and the future that says whether
    /// all of it came; bytes get their `content-length`.
    fn into_wit(self, headers: &mut http::HeaderMap) -> (StreamReader<u8>, FutureReader<Trailers>) {
        // Dropped unwritten, `end` says all of it came.
        let (end, ended) = wit_future::new(|| Ok(None));
        let (mut tx, rx) = wit_stream::new();
        // Written by a task of its own: the host reads a body only after it
        // has the request or response, so writing first would wait forever.
        match self.0 {
            Inner::Bytes(bytes) => {
                headers.insert(http::header::CONTENT_LENGTH, bytes.len().into());
                wasip3::spawn_local(async move {
                    let _ = tx.write_all(bytes).await;
                });
            }
            Inner::Stream(write) => wasip3::spawn_local(async move {
                // The body ends when `write` drops its writer; then `end`
                // says whether that was all of it.
                if let Err(Error(why)) = write(BodyWriter(tx)).await {
                    let _ = end.write(Err(wit::ErrorCode::InternalError(Some(why)))).await;
                }
            }),
        }
        (rx, ended)
    }
}

impl Default for Body {
    fn default() -> Body {
        Body::empty()
    }
}

impl std::ops::Deref for Body {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl AsRef<[u8]> for Body {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl From<Vec<u8>> for Body {
    fn from(bytes: Vec<u8>) -> Body {
        Body(Inner::Bytes(bytes))
    }
}

impl From<&[u8]> for Body {
    fn from(bytes: &[u8]) -> Body {
        bytes.to_vec().into()
    }
}

impl From<String> for Body {
    fn from(text: String) -> Body {
        text.into_bytes().into()
    }
}

impl From<&str> for Body {
    fn from(text: &str) -> Body {
        text.as_bytes().into()
    }
}

impl From<()> for Body {
    fn from((): ()) -> Body {
        Body::empty()
    }
}

impl fmt::Debug for Body {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Inner::Bytes(b) => write!(f, "Body({} bytes)", b.len()),
            Inner::Stream(_) => f.write_str("Body(stream)"),
        }
    }
}

/// What a streamed body ([`Body::stream`]) is written with.
pub struct BodyWriter(StreamWriter<u8>);

impl BodyWriter {
    /// Sends `data` on its way; `false` once the client has gone.
    pub async fn write(&mut self, data: impl Into<Vec<u8>>) -> bool {
        self.0.write_all(data.into()).await.is_empty()
    }
}

/// One Server-Sent Event, `data` split into `data:` lines, for a
/// [`Body::stream`] of content type `text/event-stream`.
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

/// `headers` as `wasi:http`'s, repeated names and bytes that aren't UTF-8 kept.
fn fields(headers: http::HeaderMap) -> core::result::Result<wit::Fields, wit::ErrorCode> {
    wit::Fields::try_from(headers).map_err(|e| wit::ErrorCode::InternalError(Some(format!("{e:?}"))))
}

type Trailers = core::result::Result<Option<wit::Fields>, wit::ErrorCode>;

#[cfg(test)]
mod tests {
    use super::*;

    fn request(uri: &str, headers: &[(&str, &[u8])]) -> http::Request<Body> {
        let mut req = http::Request::builder().uri(uri);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        req.body(Body::empty()).unwrap()
    }

    #[test]
    fn server_sent_events() {
        assert_eq!(sse(Some("7"), Some("tick"), "a"), "id: 7\nevent: tick\ndata: a\n\n");
        assert_eq!(sse(None, None, "two\r\nlines"), "data: two\ndata: lines\n\n", "a line per data line");
        assert_eq!(sse(Some("1\n2"), None, ""), "id: 12\ndata: \n\n", "fields can't end the event early");
    }

    #[test]
    fn cookies() {
        let req = request("/", &[("cookie", b"a=1; voter=x7=; b"), ("cookie", br#"c="quoted""#), ("cookie", b"d=\xff")]);
        assert_eq!(req.cookie("a"), Some("1"));
        assert_eq!(req.cookie("voter"), Some("x7="), "values may hold `=`");
        assert_eq!(req.cookie("c"), Some("quoted"), "every cookie header is read");
        assert_eq!((req.cookie("b"), req.cookie("A"), req.cookie("d")), (None, None, None), "a header that isn't text is skipped");
        assert_eq!(request("/", &[]).cookie("a"), None);
    }

    #[test]
    fn query_parameters() {
        let req = request("/p?name=J%C3%B8rn+Q&n=7&n=8&flag&bad=%zz&bin=%FF", &[]);
        assert_eq!(req.query("name").as_deref(), Some("Jørn Q"), "percent-decoded, `+` a space");
        assert_eq!(req.query("n").as_deref(), Some("8"), "the last of several");
        assert_eq!((req.query("flag").as_deref(), req.query("missing")), (Some(""), None));
        assert_eq!(req.query("bad").as_deref(), Some("%zz"), "a stray `%` stays");
        assert_eq!(req.query("bin").as_deref(), Some("\u{fffd}"), "bytes that aren't UTF-8 are replaced");
        assert_eq!((req.query_or("n", 0u32), req.query_or("name", 5u32), req.query_or("none", 5u32)), (8, 5, 5));
        assert_eq!(request("/p", &[]).query("n"), None);
    }

    #[test]
    fn bodies() {
        let body = Body::from("héllo");
        assert_eq!((body.text().unwrap(), body.len(), &body[..2]), ("héllo", 6, &b"h\xc3"[..]));
        assert!(Body::from(&b"\xff"[..]).text().is_err(), "text is UTF-8 or an error");
        assert_eq!(Body::from(vec![1, 2]).into_bytes(), [1, 2]);
        assert_eq!((Body::empty().len(), Body::from(()).len(), Body::default().len()), (0, 0, 0));
        let stream = Body::stream(|_| async { Ok(()) });
        assert_eq!((stream.len(), stream.is_stream(), format!("{stream:?}")), (0, true, "Body(stream)".into()), "a stream has no bytes yet");
        assert_eq!(format!("{:?}", Body::from("ab")), "Body(2 bytes)");
    }

    #[cfg(feature = "json")]
    #[test]
    fn json_bodies() {
        let body = Body::from(r#"{"a":[1,2]}"#);
        assert_eq!(body.json::<serde_json::Value>().unwrap()["a"][1], 2);
        assert!(Body::from("{").json::<serde_json::Value>().is_err());
    }

    #[test]
    fn builder_errors_fail_the_request() {
        let res: Result = (|| Ok(http::Response::builder().header("bad name", "x").body(Body::empty())?))();
        assert_eq!(res.unwrap_err().to_string(), "invalid HTTP header name");
    }
}
