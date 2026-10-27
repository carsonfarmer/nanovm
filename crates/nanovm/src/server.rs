//! HTTP/1.1 front end: `/{app}/{object}/{rest}` goes to the object, and
//! `/_nanovm/*` serves the dashboard, its JSON API and the demo load tools.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

use bytes::Bytes;
use http::{Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde_json::json;
use tokio::net::TcpListener;

use crate::metrics::{Poison, rss_bytes};
use crate::object::{RespBody, error_response};
use crate::table::Node;

const DASHBOARD: &str = include_str!("../../../dashboard/index.html");

pub async fn serve(node: Arc<Node>, listener: TcpListener) -> anyhow::Result<()> {
    loop {
        let (sock, _) = listener.accept().await?;
        let _ = sock.set_nodelay(true);
        let node = node.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let node = node.clone();
                async move { Ok::<_, Infallible>(handle(node, req).await) }
            });
            let _ = http1::Builder::new().keep_alive(true).serve_connection(TokioIo::new(sock), svc).await;
        });
    }
}

async fn handle(node: Arc<Node>, req: Request<Incoming>) -> Response<RespBody> {
    let path = req.uri().path().to_string();
    if let Some(rest) = path.strip_prefix("/_nanovm") {
        return admin(&node, rest, &req).await;
    }
    let mut parts = path.trim_start_matches('/').splitn(3, '/');
    let (Some(app), Some(id)) = (parts.next(), parts.next()) else {
        return error_response(404, "not-found", "routes are /{app}/{object}/{path}");
    };
    if app.is_empty() || id.is_empty() {
        return error_response(404, "not-found", "routes are /{app}/{object}/{path}");
    }
    let Some(a) = node.apps.get(app) else {
        return error_response(404, "no-such-app", app);
    };
    let rest = format!("/{}", parts.next().unwrap_or(""));
    let pq = match req.uri().query() {
        Some(q) => format!("{rest}?{q}"),
        None => rest,
    };
    let (app, id) = (app.to_string(), id.to_string());

    let limit = (a.cfg.limits.max_request_body_kb << 10) as usize;
    let (mut parts, body) = req.into_parts();
    let body = match Limited::new(body, limit).collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) if e.downcast_ref::<http_body_util::LengthLimitError>().is_some() => {
            return error_response(413, "request-too-large", &format!("max_request_body_kb is {}", limit >> 10));
        }
        Err(e) => return error_response(400, "bad-request-body", &e.to_string()),
    };
    let host = parts.headers.get(http::header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("localhost");
    parts.uri = match format!("http://{host}{pq}").parse() {
        Ok(u) => u,
        Err(_) => return error_response(400, "bad-uri", &pq),
    };
    node.dispatch(&app, &id, Request::from_parts(parts, body)).await
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

fn query(req: &Request<Incoming>) -> HashMap<String, String> {
    req.uri()
        .query()
        .unwrap_or("")
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

async fn admin(node: &Arc<Node>, path: &str, req: &Request<Incoming>) -> Response<RespBody> {
    let q = query(req);
    let num = |k: &str, d: u64| q.get(k).and_then(|v| v.parse().ok()).unwrap_or(d);
    match path {
        "" => Response::builder()
            .status(StatusCode::MOVED_PERMANENTLY)
            .header("location", "/_nanovm/")
            .body(full(""))
            .unwrap(),
        "/" | "/index.html" => {
            let html = match &node.cfg.dashboard {
                Some(dir) => std::fs::read_to_string(dir.join("index.html")).unwrap_or_else(|_| DASHBOARD.into()),
                None => DASHBOARD.into(),
            };
            Response::builder()
                .header("content-type", "text/html; charset=utf-8")
                .header("cache-control", "no-store")
                .body(full(html))
                .unwrap()
        }
        "/stats" => ok_json(stats(node)),
        "/objects" => {
            let (objs, _) = node.objects(num("limit", 200) as usize);
            ok_json(json!(objs))
        }
        "/events" => ok_json(json!(node.metrics.events_since(num("since", 0)))),
        "/grid" => ok_json(json!({ "cells": node.grid(), "max": node.cfg.max_live_objects })),
        "/demo/spawn" | "/demo/hammer" | "/demo/storm" if !node.cfg.demo => error_response(404, "demo-disabled", "set demo = true"),
        "/demo/spawn" if req.method() == Method::POST => {
            // `n` fresh objects, one request each: measures cold starts.
            let app = q.get("app").cloned().unwrap_or("counter".into());
            let prefix = q.get("prefix").cloned().unwrap_or_else(|| format!("s{}", node.next_seq()));
            let n = num("n", 10_000);
            let conc = num("concurrency", 256);
            ok_json(load(node, &app, n, conc, "GET", "/", move |i| format!("{prefix}-{i}")).await)
        }
        "/demo/hammer" if req.method() == Method::POST => {
            // `n` requests spread over `objects` existing objects: warm path.
            let app = q.get("app").cloned().unwrap_or("counter".into());
            let prefix = q.get("prefix").cloned().unwrap_or("warm".into());
            let objects = num("objects", 100).max(1);
            let n = num("n", 20_000);
            let conc = num("concurrency", 256);
            let method = q.get("method").cloned().unwrap_or("GET".into());
            let path = q.get("path").cloned().unwrap_or("/".into());
            ok_json(load(node, &app, n, conc, &method, &path, move |i| format!("{prefix}-{}", i % objects)).await)
        }
        "/demo/storm" if req.method() == Method::POST => {
            let rounds = num("rounds", 10).clamp(1, 100);
            ok_json(storm(node, rounds).await)
        }
        _ => error_response(404, "not-found", path),
    }
}

/// Every way the `hostile` guest knows to misbehave, as (name, path).
const STORM: &[(&str, &str)] = &[
    ("memory", "/alloc?mb=1024"),
    ("cpu", "/spin"),
    ("timeout", "/sleep?ms=60000"),
    ("trap", "/panic"),
    ("resource-table", "/leak?n=1000000"),
    ("disk-bytes", "/fill?mount=tmp&mb=1024"),
    ("disk-files", "/files?mount=tmp"),
    ("write-app", "/write-app"),
    ("escape", "/escape"),
    ("egress-metadata", "/fetch?url=http://169.254.169.254/latest/meta-data/"),
    ("egress-loopback", "/fetch?url=http://localtest.me:8080/_nanovm/stats"),
];

/// Runs `rounds` of every [`STORM`] attack at once, each against its own
/// `hostile` object, and reports how each attack ended.
async fn storm(node: &Arc<Node>, rounds: u64) -> serde_json::Value {
    let t0 = Instant::now();
    let tasks: Vec<_> = (0..rounds)
        .flat_map(|r| STORM.iter().map(move |a| (r, a)))
        .map(|(r, &(name, path))| {
            let node = node.clone();
            tokio::spawn(async move {
                let req = Request::get(format!("http://localhost{path}")).body(Bytes::new()).unwrap();
                let t = Instant::now();
                let resp = node.dispatch("hostile", &format!("storm-{name}-{r}"), req).await;
                let status = resp.status().as_u16();
                let host = resp.headers().get("x-nanovm-error").and_then(|h| h.to_str().ok()).map(String::from);
                let body = resp.into_body().collect().await.map(|b| b.to_bytes()).unwrap_or_default();
                (name, status, host, body, t.elapsed())
            })
        })
        .collect();
    let mut out: Vec<serde_json::Value> = Vec::new();
    let mut by_name: HashMap<&str, (HashMap<u16, u64>, Option<String>, String, f64)> = HashMap::new();
    for t in tasks {
        let Ok((name, status, host, body, took)) = t.await else { continue };
        let e = by_name.entry(name).or_default();
        *e.0.entry(status).or_default() += 1;
        e.3 = e.3.max(took.as_secs_f64() * 1e3);
        if e.2.is_empty() {
            e.1 = host;
            e.2 = String::from_utf8_lossy(&body[..body.len().min(1024)]).into_owned();
        }
    }
    for &(name, path) in STORM {
        if let Some((status, killed_by, sample, max_ms)) = by_name.remove(name) {
            out.push(json!({
                "attack": name, "path": path, "status": status,
                "killed_by": killed_by, "sample": sample, "max_ms": max_ms,
            }));
        }
    }
    json!({ "rounds": rounds, "attacks": out, "elapsed_ms": t0.elapsed().as_secs_f64() * 1e3 })
}

/// Runs `n` requests through the node in-process (no sockets) from `conc`
/// client tasks, and reports throughput, latency and memory per live object.
async fn load(
    node: &Arc<Node>,
    app: &str,
    n: u64,
    conc: u64,
    method: &str,
    path: &str,
    object: impl Fn(u64) -> String + Send + Sync + 'static,
) -> serde_json::Value {
    let rss0 = rss_bytes();
    let live0 = node.metrics.stores_live.load(Relaxed);
    let method: Method = method.parse().unwrap_or(Method::GET);
    let next = Arc::new(AtomicU64::new(0));
    let object = Arc::new(object);
    let t0 = Instant::now();
    let clients: Vec<_> = (0..conc.clamp(1, n.max(1)))
        .map(|_| {
            let (node, next, object, method) = (node.clone(), next.clone(), object.clone(), method.clone());
            let (app, uri) = (app.to_string(), format!("http://localhost{path}"));
            tokio::spawn(async move {
                let mut out: Vec<(u16, u32)> = Vec::new();
                loop {
                    let i = next.fetch_add(1, Relaxed);
                    if i >= n {
                        return out;
                    }
                    let req = Request::builder().method(method.clone()).uri(&uri).body(Bytes::new()).unwrap();
                    let t = Instant::now();
                    let resp = node.dispatch(&app, &object(i), req).await;
                    let status = resp.status().as_u16();
                    let _ = resp.into_body().collect().await;
                    out.push((status, t.elapsed().as_micros() as u32));
                }
            })
        })
        .collect();
    let mut peak_rss = rss0;
    let mut results = Vec::with_capacity(n as usize);
    for c in clients {
        results.extend(c.await.unwrap_or_default());
        peak_rss = peak_rss.max(rss_bytes());
    }
    let elapsed = t0.elapsed();
    let peak_live = node.metrics.stores_live.load(Relaxed);
    let mut by_status: HashMap<u16, u64> = HashMap::new();
    for (status, _) in &results {
        *by_status.entry(*status).or_default() += 1;
    }
    let mut lat: Vec<u32> = results.iter().map(|r| r.1).collect();
    lat.sort_unstable();
    let pct = |p: f64| lat.get(((lat.len() as f64 * p) as usize).min(lat.len().saturating_sub(1))).copied().unwrap_or(0);
    json!({
        "n": n,
        "concurrency": conc,
        "ok": results.iter().filter(|r| r.0 < 400).count(),
        "status": by_status,
        "elapsed_ms": elapsed.as_secs_f64() * 1e3,
        "rps": n as f64 / elapsed.as_secs_f64(),
        "p50_us": pct(0.5),
        "p99_us": pct(0.99),
        "max_us": lat.last().copied().unwrap_or(0),
        "live_before": live0,
        "live_after": peak_live,
        "rss_before_mb": rss0 as f64 / 1048576.0,
        "rss_peak_mb": peak_rss as f64 / 1048576.0,
        "rss_per_live_kb": if peak_live > live0 {
            (peak_rss.saturating_sub(rss0)) as f64 / 1024.0 / (peak_live - live0) as f64
        } else { 0.0 },
    })
}

fn stats(node: &Node) -> serde_json::Value {
    let m = &node.metrics;
    let (_, phases) = node.objects(0);
    let poisons: serde_json::Map<String, serde_json::Value> =
        Poison::ALL.iter().map(|p| (p.as_str().to_string(), json!(m.poisons[*p as usize].load(Relaxed)))).collect();
    let status: serde_json::Map<String, serde_json::Value> =
        (1..6).map(|i| (format!("{i}xx"), json!(m.status[i].load(Relaxed)))).collect();
    let apps: Vec<_> = node
        .apps
        .values()
        .map(|a| json!({ "name": &*a.name, "limits": {
            "memory_mb": a.cfg.limits.memory_mb,
            "cpu_ms": a.cfg.limits.cpu_ms,
            "request_timeout_ms": a.cfg.limits.request_timeout_ms,
            "data_mb": a.cfg.limits.data_mb,
            "tmp_mb": a.cfg.limits.tmp_mb,
            "max_files": a.cfg.limits.max_files,
            "max_resources": a.cfg.limits.max_resources,
            "egress": a.cfg.limits.egress,
        }}))
        .collect();
    json!({
        "uptime_ms": node.started.elapsed().as_millis() as u64,
        "rss_mb": rss_bytes() as f64 / 1048576.0,
        "live_objects": node.live_objects(),
        "max_live_objects": node.cfg.max_live_objects,
        "stores_live": m.stores_live.load(Relaxed),
        "stores_peak": m.stores_peak.load(Relaxed),
        "phases": phases,
        "requests": m.requests.load(Relaxed),
        "status": status,
        "instantiations": m.instantiations.load(Relaxed),
        "instantiate_failures": m.instantiate_failures.load(Relaxed),
        "poisons": poisons,
        "evicted_idle": m.evicted_idle.load(Relaxed),
        "evicted_lru": m.evicted_lru.load(Relaxed),
        "rejected_busy": m.rejected_busy.load(Relaxed),
        "rejected_capacity": m.rejected_capacity.load(Relaxed),
        "egress_allowed": m.egress_allowed.load(Relaxed),
        "egress_denied": m.egress_denied.load(Relaxed),
        "fs_denied": m.fs_denied.load(Relaxed),
        "guest_cpu_ms": m.guest_cpu_ns.load(Relaxed) / 1_000_000,
        "latency": m.latency.snapshot(),
        "instantiate": m.instantiate.snapshot(),
        "apps": apps,
        "demo": node.cfg.demo,
    })
}
