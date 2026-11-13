//! Outbound HTTP: [`fetch`] reaches the internet (hosts both the app's
//! `egress` list and the node's operator allow), and [`object`] another
//! object of this app.

use http::{Method, Request, Response, StatusCode};
use wasip3::http::types::{self as wit, ErrorCode};
use wasip3::wit_future;
use wit_bindgen::rt::async_support::{FutureReader, FutureWriter, StreamReader, StreamResult};

use crate::Body;

/// Sends `request`, to an `http://` or `https://` URL, and reads the whole
/// answer:
/// `fetch(Request::post(url).header("content-type", "application/json").body(json)?)`.
/// A body that doesn't arrive whole (the connection broke, or closed before
/// the `content-length` or the last chunk) is an error, not a short body.
pub async fn fetch(request: Request<impl Into<Body>>) -> Result<Response<Body>, ErrorCode> {
    let (parts, body) = fetch_streaming(request).await?.into_parts();
    Ok(Response::from_parts(parts, body.all().await?.into()))
}

/// [`fetch`], returning once the answer's headers arrive, its body to be
/// read as it comes ([`Streaming::next`]): to pass a long or slow answer on
/// (a [`Body::stream`]) without holding it all. Its body, unread, holds the
/// connection (one of `max_outbound_concurrency`) until it is dropped.
pub async fn fetch_streaming(request: Request<impl Into<Body>>) -> Result<Response<Streaming>, ErrorCode> {
    let (mut parts, body) = request.map(Into::into).into_parts();
    // A request without a body has none, rather than `content-length: 0`.
    // A streamed body that fails ends the request's body early: the host
    // fails the request, so the caller hears of it.
    let body = (!body.is_empty() || body.is_stream()).then(|| body.into_wit(&mut parts.headers, drop));
    let (contents, trailers) = match body {
        Some((contents, trailers)) => (Some(contents), trailers),
        None => (None, wit_future::new(|| Ok(None)).1),
    };
    let (request, _sent) = wit::Request::new(crate::fields(parts.headers)?, contents, trailers, None);
    let uri = parts.uri;
    let invalid = |()| ErrorCode::HttpRequestUriInvalid;
    request.set_method(&parts.method.into()).map_err(|()| ErrorCode::HttpRequestMethodInvalid)?;
    request.set_scheme(uri.scheme().map(Into::into).as_ref()).map_err(invalid)?;
    request.set_authority(uri.authority().map(|a| a.as_str())).map_err(invalid)?;
    request.set_path_with_query(uri.path_and_query().map(|p| p.as_str())).map_err(invalid)?;
    let response = wasip3::http::client::send(request).await?;
    let mut parts = Response::new(()).into_parts().0;
    parts.status = StatusCode::from_u16(response.get_status_code()).map_err(|_| ErrorCode::HttpProtocolError)?;
    parts.headers = response.get_headers().try_into()?;
    let (done, result) = wit_future::new(|| Ok(()));
    let (body, end) = wit::Response::consume_body(response, result);
    let body = Streaming { body, closed: false, end: Some(end), ended: Ok(()), _done: done };
    Ok(Response::from_parts(parts, body))
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
    /// [`fetch`] for the object: `request`'s URI is a path on it (`/…`).
    pub async fn fetch(&self, request: Request<impl Into<Body>>) -> Result<Response<Body>, ErrorCode> {
        let (mut parts, body) = request.into_parts();
        let path = parts.uri.path_and_query().map_or("/", |p| p.as_str());
        parts.uri = format!("{}{path}", self.0).parse().map_err(|_| ErrorCode::HttpRequestUriInvalid)?;
        fetch(Request::from_parts(parts, body)).await
    }

    /// A `GET` of `path` (`/…`) on the object.
    pub async fn get(&self, path: &str) -> Result<Response<Body>, ErrorCode> {
        self.send(Method::GET, path, Body::empty()).await
    }

    /// A `POST` of `body` to `path` on the object.
    pub async fn post(&self, path: &str, body: impl Into<Body>) -> Result<Response<Body>, ErrorCode> {
        self.send(Method::POST, path, body.into()).await
    }

    async fn send(&self, method: Method, path: &str, body: Body) -> Result<Response<Body>, ErrorCode> {
        let request = Request::builder().method(method).uri(format!("{}{path}", self.0)).body(body);
        fetch(request.map_err(|_| ErrorCode::HttpRequestUriInvalid)?).await
    }
}

/// An answer's body, read as it arrives ([`fetch_streaming`]).
pub struct Streaming {
    body: StreamReader<u8>,
    /// The body's stream has ended.
    closed: bool,
    /// Once it has, whether all of it came: its trailers, or an error.
    end: Option<FutureReader<Result<Option<wit::Fields>, ErrorCode>>>,
    ended: Result<(), ErrorCode>,
    _done: FutureWriter<Result<(), ErrorCode>>,
}

impl Streaming {
    /// The body's next piece, as soon as there is one. `Ok(None)` once all
    /// of it has arrived; `Err` if it can't: the connection broke or closed
    /// early (before the `content-length`, or the last chunk), so what came
    /// is only part of the answer. Dropping the `Streaming` stops reading
    /// and frees its connection.
    pub async fn next(&mut self) -> Result<Option<Vec<u8>>, ErrorCode> {
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

    /// All of the body, if all of it comes.
    async fn all(self) -> Result<Vec<u8>, ErrorCode> {
        let Streaming { body, end, _done, .. } = self;
        let body = body.collect().await;
        // Whether that was all of it is the host's to say, once it has ended.
        if let Some(end) = end {
            end.await?;
        }
        Ok(body)
    }
}
