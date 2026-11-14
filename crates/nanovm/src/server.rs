//! The node's HTTP surface: `/{object}/{rest}` goes to the object.
//! Private readiness and telemetry flush routes serve the supervisor over
//! a `link`; tests serve this surface on TCP.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http::{Request, Response};
use http_body::{Body, Frame};
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::{TcpListener, TcpStream};

use crate::object::{BoxError, RespBody, empty_response, error_response};
use crate::table::{BAD_OBJECT, Node, route};

/// Serves HTTP/1.1 on every connection `listener` accepts.
pub async fn serve_http1<F, Fut>(listener: TcpListener, f: F) -> anyhow::Result<()>
where
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Response<RespBody>> + Send + 'static,
{
    loop {
        serve_conn(accept(&listener).await.0, None, (), f.clone());
    }
}

/// The next connection on `listener`, and its peer. An error, such as
/// running out of file descriptors, is logged and waited out rather than
/// ending the server.
pub async fn accept(listener: &TcpListener) -> (TcpStream, std::net::SocketAddr) {
    loop {
        match listener.accept().await {
            Ok((sock, peer)) => {
                let _ = sock.set_nodelay(true);
                return (sock, peer);
            }
            Err(e) => {
                eprintln!("nanovm: accepting a connection: {e}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

/// The most a request's or response's head may be, at every hop: so any
/// header map the host holds for a guest, its own or a copy of one it was
/// sent, is at most this (see `object::new_store`).
pub const MAX_HEAD: usize = 64 << 10;

/// How long a client may take over a request's headers at the proxy (or
/// idle between requests), and keep a server waiting for a request's body
/// before what it sends buys more time (see [`Paced`]).
pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bytes of a request body that buy its sender another second of waiting.
const MIN_BODY_RATE: u64 = 4096;

/// A request body its sender must keep sending (N-505): the server waits
/// for it [`CLIENT_TIMEOUT`] in all plus a second for every
/// [`MIN_BODY_RATE`] bytes received, so a body trickled a byte at a time is
/// cut after about 10 s while one sent at 4 KiB/s or more never is. Only
/// time spent waiting for the sender counts, not time the server spends
/// elsewhere (a worker starting, a full socket to the next hop).
pub struct Paced<B> {
    inner: B,
    /// The waiting left, while not waiting.
    left: Duration,
    /// Set when the body last made the server wait.
    timer: Option<Pin<Box<tokio::time::Sleep>>>,
    waiting: bool,
}

/// A [`Paced`] body's sender ran out of time.
#[derive(Debug)]
pub struct BodyTimeout;

impl std::fmt::Display for BodyTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (wait, rate) = (CLIENT_TIMEOUT.as_secs(), MIN_BODY_RATE >> 10);
        write!(f, "the request body came too slowly: {wait}s of waiting, plus 1s per {rate} KiB sent")
    }
}

impl std::error::Error for BodyTimeout {}

impl<B> Paced<B> {
    pub fn new(inner: B) -> Self {
        Paced { inner, left: CLIENT_TIMEOUT, timer: None, waiting: false }
    }
}

impl<B> Body for Paced<B>
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: Into<BoxError>,
{
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = &mut *self;
        let Poll::Ready(frame) = Pin::new(&mut this.inner).poll_frame(cx) else {
            let until = tokio::time::Instant::now() + this.left;
            let timer = this.timer.get_or_insert_with(|| Box::pin(tokio::time::sleep_until(until)));
            if !std::mem::replace(&mut this.waiting, true) {
                timer.as_mut().reset(until);
            }
            return timer.as_mut().poll(cx).map(|()| Some(Err(BodyTimeout.into())));
        };
        if std::mem::take(&mut this.waiting) && let Some(t) = &this.timer {
            this.left = t.deadline().saturating_duration_since(tokio::time::Instant::now());
        }
        if let Some(Ok(f)) = &frame && let Some(d) = f.data_ref() {
            this.left += Duration::from_micros(d.len() as u64 * 1_000_000 / MIN_BODY_RATE);
        }
        Poll::Ready(frame.map(|f| f.map_err(Into::into)))
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

/// Whether `e` is, or was caused by, a [`Paced`] body running out of time.
pub fn timed_out(e: &(dyn std::error::Error + 'static)) -> bool {
    std::iter::successors(Some(e), |e| e.source()).any(|e| e.is::<BodyTimeout>())
}

/// The answer to a request whose body ran out of time; the connection
/// closes after it, as the body wasn't read to the end.
pub fn body_timeout() -> Response<RespBody> {
    error_response(408, "body-timeout", &BodyTimeout.to_string())
}

/// Serves HTTP/1.1 on one connection, in its own task, holding `guard` until
/// the connection is done. With `header_timeout`, a client that takes longer
/// to send a request's headers (or leaves the connection idle that long
/// between requests) is hung up on.
pub fn serve_conn<I, G, F, Fut, B>(io: I, header_timeout: Option<Duration>, guard: G, f: F)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    G: Send + Sync + 'static,
    F: Fn(Request<Incoming>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response<B>> + Send + 'static,
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<crate::object::BoxError>,
{
    let svc = service_fn(move |req| {
        let _ = &guard;
        let res = f(req);
        async move { Ok::<_, Infallible>(res.await) }
    });
    let mut builder = http1::Builder::new();
    builder.max_header_size(MAX_HEAD);
    if header_timeout.is_some() {
        builder.timer(TokioTimer::new()).header_read_timeout(header_timeout);
    }
    tokio::spawn(builder.serve_connection(TokioIo::new(io), svc).with_upgrades());
}

pub async fn handle<B>(node: Arc<Node>, req: Request<B>) -> Response<RespBody>
where
    B: http_body::Body<Data = Bytes> + Unpin,
    B::Error: Into<BoxError>,
{
    let path = req.uri().path().to_string();
    if let Some(rest) = path.strip_prefix("/_nanovm") {
        return admin(&node, rest, req.headers()).await;
    }
    let Some((id, rest)) = route(&path) else {
        return error_response(400, "bad-object-id", BAD_OBJECT);
    };
    let rest = if rest.is_empty() { "/" } else { rest };
    let pq = req.uri().query().map_or_else(|| rest.to_string(), |q| format!("{rest}?{q}"));
    let limit = (node.app.cfg.limits.max_request_body_kb << 10) as usize;
    let (mut parts, body) = req.into_parts();
    let body = match Limited::new(Paced::new(body), limit).collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) if timed_out(&*e) => return body_timeout(),
        Err(e) if e.is::<http_body_util::LengthLimitError>() => {
            return error_response(413, "request-too-large", &format!("max_request_body_kb is {}", limit >> 10));
        }
        Err(e) => return error_response(400, "bad-request-body", &e.to_string()),
    };
    let host = parts.headers.get(http::header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("localhost");
    let Ok(uri) = format!("http://{host}{pq}").parse() else { return error_response(400, "bad-uri", &pq) };
    parts.uri = uri;
    node.dispatch(id, Request::from_parts(parts, body)).await
}

async fn admin(node: &Arc<Node>, path: &str, headers: &http::HeaderMap) -> Response<RespBody> {
    match path {
        "/ready" => empty_response(204),
        "/lease" => {
            let session = headers.get(crate::lease::SESSION).and_then(|s| s.to_str().ok()?.split_once(' '));
            if let Some((Ok(id), Ok(until))) = session.map(|(i, u)| (i.parse(), u.parse())) {
                node.renewed(id, until);
            }
            empty_response(204)
        }
        p if p.starts_with("/evict/") => {
            node.evict(&p["/evict/".len()..]).await;
            empty_response(204)
        }
        "/flush" => {
            let telemetry = node.telemetry.clone();
            let ok = tokio::task::spawn_blocking(move || telemetry.flush()).await.is_ok_and(|r| r.is_ok());
            empty_response(if ok { 204 } else { 503 })
        }
        _ => error_response(404, "not-found", path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{Instant, sleep};

    /// A body fed by its sender: each `n` sent is a frame of n bytes, and
    /// dropping the sender ends it.
    fn body() -> (tokio::sync::mpsc::UnboundedSender<usize>, Paced<impl Body<Data = Bytes, Error = Infallible> + Unpin>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let frames = futures::stream::unfold(rx, |mut rx| async move {
            let n = rx.recv().await?;
            Some((Ok(Frame::data(Bytes::from(vec![0; n]))), rx))
        });
        (tx, Paced::new(http_body_util::StreamBody::new(Box::pin(frames))))
    }

    /// A trickled body is cut after `CLIENT_TIMEOUT`; one sent at the
    /// minimum rate or faster never is, however long it takes; time the
    /// server spends not reading isn't charged to the sender (N-505).
    #[tokio::test(start_paused = true)]
    async fn bodies_must_keep_coming() {
        let t0 = Instant::now();
        let (tx, b) = body();
        tokio::spawn(async move {
            while tx.send(1).is_ok() {
                sleep(Duration::from_secs(2)).await;
            }
        });
        let e = b.collect().await.unwrap_err();
        assert!(e.is::<BodyTimeout>(), "{e}");
        assert!((CLIENT_TIMEOUT..CLIENT_TIMEOUT + Duration::from_millis(10)).contains(&t0.elapsed()), "{:?}", t0.elapsed());

        // 600 KiB at 5 KiB/s: two minutes, never more than 0.2 s behind.
        let (tx, b) = body();
        tokio::spawn(async move {
            for _ in 0..600 {
                tx.send(1024).unwrap();
                sleep(Duration::from_millis(200)).await;
            }
        });
        let t0 = Instant::now();
        assert_eq!(b.collect().await.unwrap().to_bytes().len(), 600 << 10);
        assert!(t0.elapsed() >= Duration::from_secs(119));

        // At 2 KiB/s the budget lasts until ~40 KiB: 10 s plus 0.25 s per KiB, less 0.5 s waited per KiB.
        let (tx, b) = body();
        tokio::spawn(async move {
            while tx.send(1024).is_ok() {
                sleep(Duration::from_millis(500)).await;
            }
        });
        let t0 = Instant::now();
        assert!(b.collect().await.unwrap_err().is::<BodyTimeout>());
        assert!((Duration::from_secs(19)..Duration::from_secs(21)).contains(&t0.elapsed()), "{:?}", t0.elapsed());

        // The server reads 4 KiB, then nothing for a minute; the sender then
        // stalls: its 10 s plus 1 s start when the server next waits.
        let (tx, mut b) = body();
        tx.send(4096).unwrap();
        b.frame().await.unwrap().unwrap();
        sleep(Duration::from_secs(60)).await;
        let t0 = Instant::now();
        assert!(b.frame().await.unwrap().unwrap_err().is::<BodyTimeout>());
        assert_eq!(t0.elapsed().as_secs(), 11);
        drop(tx);
    }
}
