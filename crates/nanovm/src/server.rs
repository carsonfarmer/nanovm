//! The node's HTTP surface: `/{object}/{rest}` goes to the object, and
//! `/_nanovm/stats` and `/_nanovm/events` serve the node's metrics and event
//! log. A worker serves it to its supervisor over a `link`; tests serve it
//! on TCP.

use std::convert::Infallible;
use std::sync::Arc;

use bytes::Bytes;
use http::{Request, Response};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde_json::json;
use tokio::net::TcpListener;

use crate::metrics::rss_bytes;
use crate::object::{RespBody, error_response};
use crate::table::{BAD_OBJECT, Node, route};

/// Serves HTTP/1.1 on every connection `listener` accepts.
pub async fn serve_http1<F, Fut>(listener: TcpListener, f: F) -> anyhow::Result<()>
where
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Response<RespBody>> + Send + 'static,
{
    loop {
        let (sock, _) = listener.accept().await?;
        let _ = sock.set_nodelay(true);
        serve_conn(sock, f.clone());
    }
}

/// Serves HTTP/1.1 on one connection, in its own task.
pub fn serve_conn<I, F, Fut, B>(io: I, f: F)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    F: Fn(Request<Incoming>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response<B>> + Send + 'static,
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<crate::object::BoxError>,
{
    let svc = service_fn(move |req| {
        let res = f(req);
        async move { Ok::<_, Infallible>(res.await) }
    });
    tokio::spawn(http1::Builder::new().serve_connection(TokioIo::new(io), svc).with_upgrades());
}

pub async fn handle(node: Arc<Node>, req: Request<Incoming>) -> Response<RespBody> {
    let path = req.uri().path().to_string();
    if let Some(rest) = path.strip_prefix("/_nanovm") {
        return admin(&node, rest, &req).await;
    }
    let Some((id, rest)) = route(&path) else {
        return error_response(400, "bad-object-id", BAD_OBJECT);
    };
    let rest = if rest.is_empty() { "/" } else { rest };
    let pq = match req.uri().query() {
        Some(q) => format!("{rest}?{q}"),
        None => rest.to_string(),
    };

    let limit = (node.app.cfg.limits.max_request_body_kb << 10) as usize;
    let (mut parts, body) = req.into_parts();
    let body = match Limited::new(body, limit).collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) if e.downcast_ref::<http_body_util::LengthLimitError>().is_some() => {
            return error_response(
                413,
                "request-too-large",
                &format!("max_request_body_kb is {}", limit >> 10),
            );
        }
        Err(e) => return error_response(400, "bad-request-body", &e.to_string()),
    };
    let host = parts
        .headers
        .get(http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost");
    parts.uri = match format!("http://{host}{pq}").parse() {
        Ok(u) => u,
        Err(_) => return error_response(400, "bad-uri", &pq),
    };
    node.dispatch(id, Request::from_parts(parts, body)).await
}

pub fn ok_json(v: serde_json::Value) -> Response<RespBody> {
    Response::builder()
        .header("content-type", "application/json")
        .header("cache-control", "no-store")
        .body(full(v.to_string()))
        .unwrap()
}

fn full(b: impl Into<Bytes>) -> RespBody {
    Full::new(b.into()).map_err(|e| match e {}).boxed_unsync()
}

async fn admin(node: &Arc<Node>, path: &str, req: &Request<Incoming>) -> Response<RespBody> {
    let since = req.uri().query().and_then(|q| q.strip_prefix("since=")?.parse().ok());
    match path {
        "/stats" => ok_json(stats(node)),
        "/events" => ok_json(json!(node.metrics.events_since(since.unwrap_or(0)))),
        _ => error_response(404, "not-found", path),
    }
}

fn stats(node: &Node) -> serde_json::Value {
    let mut v = serde_json::to_value(&*node.metrics).expect("metrics serialize");
    v["uptime_ms"] = json!(node.started.elapsed().as_millis() as u64);
    v["rss_mb"] = json!(rss_bytes() as f64 / 1048576.0);
    v["live_objects"] = json!(node.live_objects());
    v["max_live_objects"] = json!(node.cfg.max_live_objects);
    v["phases"] = json!(node.phases());
    v["app"] = json!({ "name": &*node.app.name, "limits": node.app.cfg.limits });
    v
}
