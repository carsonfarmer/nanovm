//! A benign durable object: an in-memory hit counter (lost on eviction), a
//! persisted counter in `/data/count`, `/kv/{key}` on its `wasi:keyvalue`
//! store, and a chat room on `/ws`: every socket joins channel `room`, `pub
//! <text>` (or `POST /publish?msg=…&n=…`) sends to all of them, `close`
//! closes, `id` answers the socket's id, and anything else is echoed. Ended
//! sockets are counted in the key `ws-ends`.

use std::sync::atomic::{AtomicU64, Ordering};

use nanovm_sdk::{Request, Response, Result, data, fs, kv, quote, ws};

nanovm_sdk::serve!(handle);

static HITS: AtomicU64 = AtomicU64::new(0);

async fn handle(req: Request) -> Result {
    let hits = HITS.fetch_add(1, Ordering::Relaxed) + 1;
    match (req.method(), req.path()) {
        ("GET", "/") => Ok(Response::json(format!(
            r#"{{"app":{},"object":{},"instance_hits":{hits},"count":{}}}"#,
            quote(&nanovm_sdk::app()),
            quote(&nanovm_sdk::id()),
            count().await
        ))),
        ("POST", "/incr") => {
            let n = count().await + 1;
            Ok(match data().write("count", n.to_string()).await {
                Ok(()) => Response::json(format!(r#"{{"count":{n},"instance_hits":{hits}}}"#)),
                Err(e) => Response::json(format!(r#"{{"error":{}}}"#, quote(&fs::code(&e)))).status(507),
            })
        }
        ("GET", p) if p.starts_with("/config/") => Ok(match nanovm_sdk::config::get(&p[8..]) {
            Ok(Some(v)) => Response::text(v),
            Ok(None) => Response::text("no such config key").status(404),
            Err(e) => Response::text(format!("{e:?}")).status(500),
        }),
        ("POST", "/ws") if ws::events(&req).is_some() => socket(&req).await,
        ("POST", "/publish") => {
            let msg = req.query("msg").unwrap_or("");
            Ok(Response::text("").status(ws::publish_all("room", vec![msg; req.query_or("n", 1)]).await?))
        }
        (m, p) if p.starts_with("/kv/") => Ok(store(m, &p[4..], req.body())),
        _ => Ok(Response::not_found()),
    }
}

async fn count() -> u64 {
    let n = data().read_text("count").await.ok().flatten();
    n.and_then(|n| n.trim().parse().ok()).unwrap_or(0)
}

/// `/kv/{key}` on the default bucket.
fn store(method: &str, key: &str, body: &[u8]) -> Response {
    if key.is_empty() || key.len() > 64 || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Response::text("bad key").status(400);
    }
    let done = match method {
        "GET" => kv::get(key).map(|v| match v {
            Some(v) => Response::bytes("application/octet-stream", v),
            None => Response::text("no such key").status(404),
        }),
        "PUT" | "POST" => kv::set(key, body).map(|()| Response::text("").status(204)),
        "DELETE" => kv::delete(key).map(|()| Response::text("").status(204)),
        _ => return Response::text("method not allowed").status(405),
    };
    done.unwrap_or_else(|e| Response::json(format!(r#"{{"error":{}}}"#, quote(&format!("{e:?}")))).status(507))
}

async fn socket(req: &Request) -> Result {
    let id = req.header("connection-id").unwrap_or_default();
    let mut answer = ws::Answer::default();
    for event in ws::events(req).unwrap_or_default() {
        match event {
            ws::Event::Open => {
                answer.accept().subscribe("room");
            }
            ws::Event::Text(t) if t == "close" => {
                answer.close(4000);
            }
            ws::Event::Text(t) if t == "id" => {
                answer.text(id);
            }
            ws::Event::Text(t) if t.starts_with("pubpanic ") => {
                ws::publish("room", &t[9..]).await?;
                panic!("after publishing");
            }
            ws::Event::Text(t) if t.starts_with("pub ") => {
                ws::publish("room", &t[4..]).await?;
            }
            ws::Event::Text(t) => {
                answer.text(&format!("echo {t}"));
            }
            ws::Event::Binary(b) => {
                answer.binary(&b);
            }
            ws::Event::Close(_) | ws::Event::Disconnect => {
                kv::increment("ws-ends", 1)?;
            }
        }
    }
    Ok(answer.into())
}
