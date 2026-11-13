//! Outbound HTTP: [`fetch`] reaches the internet (hosts both the app's
//! `egress` list and the node's operator allow), and [`object`] another
//! object of this app.

use wasip3::http::types::{self as wit, ErrorCode, Method, Scheme};
use wasip3::wit_future;

/// A request to send: `fetch(url).method("POST").header(k, v).body(b).send()`.
pub struct Fetch {
    method: Method,
    url: String,
    headers: Vec<(String, Vec<u8>)>,
    body: Option<Vec<u8>>,
}

/// A `GET` of `url`, an `http://` or `https://` URL.
pub fn fetch(url: impl Into<String>) -> Fetch {
    Fetch { method: Method::Get, url: url.into(), headers: vec![], body: None }
}

/// Another object of this app, like a Durable Object stub. A call reaches it
/// through its queue, so it is served one request at a time as always, and
/// it commits before it answers. A call that would wait on its own caller
/// (`a` calls `b`, which calls `a`) is refused with `508` at once.
pub fn object(id: &str) -> Object {
    Object(format!("http://nanovm/objects/{id}"))
}

pub struct Object(String);

impl Object {
    /// A `GET` of `path` (`/…`) on the object.
    pub fn get(&self, path: &str) -> Fetch {
        fetch(format!("{}{path}", self.0))
    }

    /// A `POST` of `body` to `path` on the object.
    pub fn post(&self, path: &str, body: impl Into<Vec<u8>>) -> Fetch {
        self.get(path).method("POST").body(body)
    }
}

impl Fetch {
    pub fn method(mut self, method: &str) -> Fetch {
        self.method = match method {
            "GET" => Method::Get,
            "POST" => Method::Post,
            "PUT" => Method::Put,
            "DELETE" => Method::Delete,
            "PATCH" => Method::Patch,
            "HEAD" => Method::Head,
            other => Method::Other(other.into()),
        };
        self
    }

    pub fn header(mut self, name: &str, value: impl AsRef<[u8]>) -> Fetch {
        self.headers.push((name.into(), value.as_ref().into()));
        self
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Fetch {
        self.body = Some(body.into());
        self
    }

    /// Sends the request and reads the whole answer. A body that doesn't
    /// arrive whole (the connection broke, or closed before the
    /// `content-length` or the last chunk) is an error, not a short body.
    pub async fn send(self) -> Result<Reply, ErrorCode> {
        let Streaming { status, headers, body, end, _done, .. } = self.stream().await?;
        let body = body.collect().await;
        // Whether that was all of it is the host's to say, once it has ended.
        if let Some(end) = end {
            end.await?;
        }
        Ok(Reply { status, headers, body })
    }

    /// Sends the request and returns once the answer's headers arrive, its
    /// body to be read as it comes ([`Streaming::next`]): to pass a long or
    /// slow answer on (`Response::stream`) without holding it all.
    pub async fn stream(self) -> Result<Streaming, ErrorCode> {
        let response = self.open().await?;
        let status = response.get_status_code();
        let headers = response.get_headers().copy_all().into_iter();
        let headers = headers.map(|(k, v)| (k, String::from_utf8_lossy(&v).into_owned())).collect();
        let (done, result) = wit_future::new(|| Ok(()));
        let (body, end) = wit::Response::consume_body(response, result);
        Ok(Streaming { status, headers, body, closed: false, end: Some(end), ended: Ok(()), _done: done })
    }

    /// Sends the request and returns once the answer's headers arrive. Its
    /// body, unread, holds the connection (one of `max_outbound_concurrency`)
    /// until the response is dropped.
    pub async fn open(self) -> Result<wit::Response, ErrorCode> {
        let (scheme, rest) = match self.url.split_once("://") {
            Some(("https", rest)) => (Scheme::Https, rest),
            Some(("http", rest)) => (Scheme::Http, rest),
            _ => return Err(ErrorCode::HttpRequestUriInvalid),
        };
        let (authority, path) = rest.find('/').map_or((rest, "/"), |i| rest.split_at(i));
        let headers = crate::fields(&self.headers)?;
        let (request, _sent) = wit::Request::new(headers, self.body.map(crate::stream), crate::no_trailers(), None);
        let _ = request.set_method(&self.method);
        let _ = request.set_scheme(Some(&scheme));
        let _ = request.set_authority(Some(authority));
        let _ = request.set_path_with_query(Some(path));
        wasip3::http::client::send(request).await
    }
}

/// An answer whose body is read as it arrives ([`Fetch::stream`]).
pub struct Streaming {
    pub status: u16,
    /// Names as the server sent them.
    pub headers: Vec<(String, String)>,
    body: wit_bindgen::rt::async_support::StreamReader<u8>,
    /// The body's stream has ended.
    closed: bool,
    /// Once it has, whether all of it came: its trailers, or an error.
    end: Option<wit_bindgen::rt::async_support::FutureReader<Result<Option<wit::Fields>, ErrorCode>>>,
    ended: Result<(), ErrorCode>,
    _done: wit_bindgen::rt::async_support::FutureWriter<Result<(), ErrorCode>>,
}

impl Streaming {
    /// The body's next piece, as soon as there is one. `Ok(None)` once all
    /// of it has arrived; `Err` if it can't: the connection broke or closed
    /// early (before the `content-length`, or the last chunk), so what came
    /// is only part of the answer. Dropping the `Streaming` stops reading
    /// and frees its connection.
    pub async fn next(&mut self) -> Result<Option<Vec<u8>>, ErrorCode> {
        use wit_bindgen::rt::async_support::StreamResult;
        while !self.closed {
            let (result, piece) = self.body.read(Vec::with_capacity(16 << 10)).await;
            self.closed = !matches!(result, StreamResult::Complete(_));
            if !piece.is_empty() {
                return Ok(Some(piece));
            }
        }
        if let Some(end) = self.end.take() {
            self.ended = end.await.map(drop);
        }
        self.ended.clone().map(|()| None)
    }
}

/// An answer, read in full.
pub struct Reply {
    pub status: u16,
    /// Names as the server sent them.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}
