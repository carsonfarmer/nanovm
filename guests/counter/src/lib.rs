//! A benign durable object: an in-memory hit counter (lost on eviction), a
//! persisted counter in `/data/count` and `/kv/{key}` on its `wasi:keyvalue` store.

use std::sync::atomic::{AtomicU64, Ordering};

use util::wasip3;
use util::{Req, js, json, read_req, text};
use wasip3::http::types::{ErrorCode, Request, Response};

wasip3::http::service::export!(Counter);

struct Counter;

static HITS: AtomicU64 = AtomicU64::new(0);

impl wasip3::exports::http::handler::Guest for Counter {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let req = read_req(request).await;
        let hits = HITS.fetch_add(1, Ordering::Relaxed) + 1;
        route(req, hits).await
    }
}

async fn count() -> u64 {
    match util::read_file("/data", "count").await {
        Ok(b) => String::from_utf8_lossy(&b).trim().parse().unwrap_or(0),
        Err(_) => 0,
    }
}

fn valid_key(k: &str) -> bool {
    !k.is_empty() && k.len() <= 64 && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

async fn route(req: Req, hits: u64) -> Result<Response, ErrorCode> {
    let path = req.path.clone();
    match (req.method.as_str(), path.as_str()) {
        ("GET", "/") => {
            let n = count().await;
            json(
                200,
                format!(
                    "{{\"app\":{},\"object\":{},\"instance_hits\":{},\"count\":{}}}",
                    js(&util::env("NANOVM_APP")),
                    js(&util::env("NANOVM_OBJECT_ID")),
                    hits,
                    n
                ),
            )
        }
        ("POST", "/incr") => {
            let n = count().await + 1;
            match util::write_file_atomic("/data", "count", n.to_string().into_bytes()).await {
                Ok(()) => json(200, format!("{{\"count\":{n},\"instance_hits\":{hits}}}")),
                Err(e) => json(507, format!("{{\"error\":{}}}", js(&util::fs_err(&e)))),
            }
        }
        ("GET", "/static") => match util::read_file("/app", "hello.txt").await {
            Ok(b) => util::respond(200, "text/plain; charset=utf-8", b),
            Err(e) => text(404, util::fs_err(&e)),
        },
        (m, p) if p.starts_with("/kv/") => {
            let key = &p[4..];
            if !valid_key(key) {
                return text(400, "bad key");
            }
            kv(m, key, req.body).await
        }
        _ => text(404, "not found"),
    }
}

/// The object's `wasi:keyvalue` default bucket.
async fn kv(method: &str, key: &str, body: Vec<u8>) -> Result<Response, ErrorCode> {
    use util::kv::store;
    let bucket = match store::open("") {
        Ok(b) => b,
        Err(e) => return text(500, format!("{e:?}")),
    };
    let fail = |e: store::Error| json(507, format!("{{\"error\":{}}}", js(&format!("{e:?}"))));
    match method {
        "GET" => match bucket.get(key) {
            Ok(Some(v)) => util::respond(200, "application/octet-stream", v),
            Ok(None) => text(404, "no such key"),
            Err(e) => fail(e),
        },
        "PUT" | "POST" => match bucket.set(key, &body) {
            Ok(()) => text(204, ""),
            Err(e) => fail(e),
        },
        "DELETE" => match bucket.delete(key) {
            Ok(()) => text(204, ""),
            Err(e) => fail(e),
        },
        _ => text(405, "method not allowed"),
    }
}
