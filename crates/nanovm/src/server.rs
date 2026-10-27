//! HTTP/1.1 front end: `/{app}/{object}/{rest}` goes to the object, and
//! `/_nanovm/*` serves the dashboard, its JSON API and the demo load tools.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
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

use crate::load::{Run, request};
use crate::metrics::rss_bytes;
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
            let _ = http1::Builder::new()
                .keep_alive(true)
                .serve_connection(TokioIo::new(sock), svc)
                .await;
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
        "/demo/spawn" | "/demo/hammer" | "/demo/storm" if !node.cfg.demo => {
            error_response(404, "demo-disabled", "set demo = true")
        }
        "/demo/spawn" if req.method() == Method::POST => {
            // `n` fresh objects, one request each: measures cold starts.
            let app = q.get("app").cloned().unwrap_or("counter".into());
            let prefix = q
                .get("prefix")
                .cloned()
                .unwrap_or_else(|| format!("s{}", node.next_seq()));
            let (n, conc) = (num("n", 10_000) as usize, num("concurrency", 256) as usize);
            ok_json(load(node, &app, n, conc, "GET", "/", move |i| format!("{prefix}-{i}")).await)
        }
        "/demo/hammer" if req.method() == Method::POST => {
            // `n` requests spread over `objects` existing objects: warm path.
            let app = q.get("app").cloned().unwrap_or("counter".into());
            let prefix = q.get("prefix").cloned().unwrap_or("warm".into());
            let objects = num("objects", 100).max(1) as usize;
            let (n, conc) = (num("n", 20_000) as usize, num("concurrency", 256) as usize);
            let method = q.get("method").cloned().unwrap_or("GET".into());
            let path = q.get("path").cloned().unwrap_or("/".into());
            ok_json(
                load(node, &app, n, conc, &method, &path, move |i| {
                    format!("{prefix}-{}", i % objects)
                })
                .await,
            )
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
                (
                    name,
                    request(&node, "GET", "hostile", &format!("storm-{name}-{r}"), path, "").await,
                )
            })
        })
        .collect();
    let mut by_name: HashMap<&str, (Run, Option<String>, String)> = HashMap::new();
    for t in tasks {
        let Ok((name, reply)) = t.await else { continue };
        let e = by_name.entry(name).or_default();
        e.0.add(&reply);
        if e.2.is_empty() {
            e.1 = reply.error;
            e.2 = reply.body.chars().take(1024).collect();
        }
    }
    let out: Vec<_> = STORM
        .iter()
        .filter_map(|&(name, path)| {
            let (run, killed_by, sample) = by_name.remove(name)?;
            let max_ms = run.lat_us.iter().max().map_or(0.0, |&us| us as f64 / 1e3);
            Some(json!({
                "attack": name, "path": path, "status": run.status,
                "killed_by": killed_by, "sample": sample, "max_ms": max_ms,
            }))
        })
        .collect();
    json!({ "rounds": rounds, "attacks": out, "elapsed_ms": t0.elapsed().as_secs_f64() * 1e3 })
}

/// Runs `n` requests through the node in-process (see [`crate::load`]) and
/// reports throughput, latency and memory per live object.
async fn load(
    node: &Arc<Node>,
    app: &str,
    n: usize,
    conc: usize,
    method: &str,
    path: &str,
    object: impl Fn(usize) -> String + Send + Sync + 'static,
) -> serde_json::Value {
    let (rss0, live0) = (rss_bytes(), node.metrics.stores_live.load(Relaxed));
    let r = crate::load::run(node, app, method, path, n, None, conc, object).await;
    let (rss1, live1) = (rss_bytes().max(rss0), node.metrics.stores_live.load(Relaxed));
    let us = |q: f64| (r.pct(q) * 1e3).round() as u64;
    let grown = (live1 - live0).max(0) as f64;
    json!({
        "n": n,
        "concurrency": conc,
        "ok": r.status.iter().filter(|&(s, _)| *s < 400).map(|(_, c)| c).sum::<usize>(),
        "status": r.status,
        "elapsed_ms": r.elapsed.as_secs_f64() * 1e3,
        "rps": n as f64 / r.elapsed.as_secs_f64(),
        "p50_us": us(0.5),
        "p99_us": us(0.99),
        "max_us": r.lat_us.last().copied().unwrap_or(0),
        "live_before": live0,
        "live_after": live1,
        "rss_before_mb": rss0 as f64 / 1048576.0,
        "rss_peak_mb": rss1 as f64 / 1048576.0,
        "rss_per_live_kb": if grown > 0.0 { (rss1 - rss0) as f64 / 1024.0 / grown } else { 0.0 },
    })
}

fn stats(node: &Node) -> serde_json::Value {
    let (_, phases) = node.objects(0);
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
            "demo": node.cfg.demo,
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    v
}
