//! A benign durable object: an in-memory hit counter (lost on eviction), a
//! persisted counter in `/data/count`, `/kv/{key}` on its `wasi:keyvalue`
//! store, and a chat room on `/ws`: every socket joins channel `room`, `pub
//! <text>` (or `POST /publish?msg=…&n=…`) sends to all of them, `close`
//! closes, `id` answers the socket's id, and anything else is echoed. Ended
//! sockets are counted in the key `ws-ends`.

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
        ("GET", p) if p.starts_with("/config/") => match util::config::get(&p[8..]) {
            Ok(Some(v)) => text(200, v),
            Ok(None) => text(404, "no such config key"),
            Err(e) => text(500, format!("{e:?}")),
        },
        ("POST", "/ws") if req.headers.get("content-type").is_some_and(|t| t == "application/websocket-events") => {
            ws(req).await
        }
        ("POST", "/publish") => {
            let msg = req.q("msg").unwrap_or("").to_string();
            let n = req.q_u64("n", 1) as usize;
            text(publish(&vec![msg; n]).await?, "")
        }
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

/// Pushpin's WebSocket-over-HTTP events: `KIND\r\n` or `KIND HEXLEN\r\n`
/// with that much content and `\r\n`.
fn events(mut body: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut out = vec![];
    while let Some(end) = body.windows(2).position(|w| w == b"\r\n") {
        let head = String::from_utf8_lossy(&body[..end]).into_owned();
        body = &body[end + 2..];
        let (kind, content) = match head.split_once(' ') {
            Some((k, n)) => {
                let n = usize::from_str_radix(n, 16).unwrap_or(0).min(body.len());
                let c = body[..n].to_vec();
                body = &body[(n + 2).min(body.len())..];
                (k.to_string(), c)
            }
            None => (head, vec![]),
        };
        out.push((kind, content));
    }
    out
}

fn event(kind: &str, content: &[u8]) -> Vec<u8> {
    [format!("{kind} {:x}\r\n", content.len()).as_bytes(), content, b"\r\n"].concat()
}

async fn ws(req: Req) -> Result<Response, ErrorCode> {
    let id = req.headers.get("connection-id").cloned().unwrap_or_default();
    let mut out = vec![];
    for (kind, content) in events(&req.body) {
        let text = String::from_utf8_lossy(&content).into_owned();
        match kind.as_str() {
            "OPEN" => {
                out.extend(b"OPEN\r\n");
                out.extend(event("TEXT", br#"c:{"type":"subscribe","channel":"room"}"#));
            }
            "TEXT" if text == "close" => out.extend(event("CLOSE", &4000u16.to_be_bytes())),
            "TEXT" if text == "id" => out.extend(event("TEXT", format!("m:{id}").as_bytes())),
            "TEXT" if text.starts_with("pubpanic ") => {
                publish(&[text[9..].to_string()]).await?;
                panic!("after publishing");
            }
            "TEXT" if text.starts_with("pub ") => {
                publish(&[text[4..].to_string()]).await?;
            }
            "TEXT" => out.extend(event("TEXT", format!("m:echo {text}").as_bytes())),
            "BINARY" => out.extend(event("BINARY", &content)),
            _ => {
                let bucket = util::kv::store::open("").map_err(|e| ErrorCode::InternalError(Some(format!("{e:?}"))))?;
                let _ = util::kv::atomics::increment(&bucket, "ws-ends", 1);
            }
        }
    }
    let headers = [("content-type", "application/websocket-events"), ("sec-websocket-extensions", "grip")];
    util::respond_with(200, &headers, out)
}

/// Sends each of `msgs` to every socket in `room`; the status.
async fn publish(msgs: &[String]) -> Result<u16, ErrorCode> {
    let items: Vec<String> = msgs
        .iter()
        .map(|m| format!(r#"{{"channel":"room","formats":{{"ws-message":{{"content":{}}}}}}}"#, js(m)))
        .collect();
    util::post("nanovm", "/publish/", format!(r#"{{"items":[{}]}}"#, items.join(",")).into_bytes()).await
}
