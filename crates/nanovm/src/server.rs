//! The node's HTTP surface: `/{app}/{object}/{rest}` goes to the object, and
//! `/_nanovm/stats` and `/_nanovm/events` serve the node's metrics and event
//! log. In-process nodes serve it on TCP; a worker serves it to its
//! supervisor over a `link`.

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
use crate::table::Node;

pub async fn serve(node: Arc<Node>, listener: TcpListener) -> anyhow::Result<()> {
    serve_http1(listener, move |req| handle(node.clone(), req)).await
}

/// Serves HTTP/1.1 on every connection `listener` accepts.
pub async fn serve_http1<F, Fut>(listener: TcpListener, f: F) -> anyhow::Result<()>
where
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Response<RespBody>> + Send + 'static,
{
    loop {
        let (sock, _) = listener.accept().await?;
        let _ = sock.set_nodelay(true);
        let f = f.clone();
        let svc = service_fn(move |req| {
            let res = f(req);
            async move { Ok::<_, Infallible>(res.await) }
        });
        tokio::spawn(http1::Builder::new().keep_alive(true).serve_connection(TokioIo::new(sock), svc));
    }
}

pub async fn handle(node: Arc<Node>, req: Request<Incoming>) -> Response<RespBody> {
    let path = req.uri().path().to_string();
    if let Some(rest) = path.strip_prefix("/_nanovm") {
        return admin(&node, rest, &req).await;
    }
    let mut parts = path.trim_start_matches('/').splitn(3, '/');
    // `dispatch` rejects an empty or invalid object id.
    let (Some(app), Some(id)) = (parts.next(), parts.next()) else {
        return error_response(404, "not-found", "routes are /{app}/{object}/{path}");
    };
    let Some(a) = node.apps.get(app) else {
        return error_response(404, "no-such-app", app);
    };
    let rest = format!("/{}", parts.next().unwrap_or(""));
    let pq = match req.uri().query() {
        Some(q) => format!("{rest}?{q}"),
        None => rest,
    };

    let limit = (a.cfg.limits.max_request_body_kb << 10) as usize;
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
    node.dispatch(app, id, Request::from_parts(parts, body)).await
}

fn ok_json(v: serde_json::Value) -> Response<RespBody> {
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
    let phases = node.phases();
    let apps: Vec<_> = node
        .apps
        .values()
        .map(|a| json!({ "name": &*a.name, "limits": a.cfg.limits }))
        .collect();
    let mut v = serde_json::to_value(&*node.metrics).expect("metrics serialize");
    v.as_object_mut().unwrap().extend(
        json!({
            "uptime_ms": node.started.elapsed().as_millis() as u64,
            "rss_mb": rss_bytes() as f64 / 1048576.0,
            "live_objects": node.live_objects(),
            "max_live_objects": node.cfg.max_live_objects,
            "phases": phases,
            "apps": apps,
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    v
}
