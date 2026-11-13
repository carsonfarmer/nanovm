//! Presence and typing indicators. Each room is an object (`/{room}/`);
//! everyone in it has a WebSocket, and the room tells them who comes, who
//! goes and who is typing. Who is here is one key per socket in the room's
//! key-value store, so it survives the room's instance being evicted.
//!
//! - `/{room}/ws?name=ann`: a WebSocket. It gets `{"type":"here","names":[…]}`,
//!   then `join`, `leave` and `typing` events. It sends `typing` while its
//!   person types, and `ping` every so often to stay present.
//! - `GET /{room}/who`: `{"names":[…]}`
//!
//! A socket that ends says so (`CLOSE` or `DISCONNECT`), but one that dies
//! with its proxy or node never does: presence that isn't refreshed by a
//! `ping` within `stale_ms` (config) lapses.

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use nanovm_sdk::http::header::CONTENT_TYPE;
use nanovm_sdk::http::{Request, Response};
use nanovm_sdk::{Body, RequestExt, Result, config, kv, serde_json, ws};
use serde::{Deserialize, Serialize};
use serde_json::json;

nanovm_sdk::serve!(handle);

const CHANNEL: &str = "room";

#[derive(Serialize, Deserialize)]
struct Here {
    name: String,
    seen: u64,
}

async fn handle(req: Request<Body>) -> Result {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    let Some(events) = ws::events(&req) else {
        return Ok(match (req.method().as_str(), req.uri().path()) {
            ("GET", "/who") => json(200, json!({ "names": present(now).await? }).to_string())?,
            _ => text(404, "not found")?,
        });
    };
    // The proxy names each socket; only it can send these requests.
    let key = format!("socket:{}", req.headers().get("connection-id").and_then(|v| v.to_str().ok()).unwrap_or_default());
    let name = req.query("name").unwrap_or_default().trim().to_string();
    if events.contains(&ws::Event::Open) && !(1..=32).contains(&name.chars().count()) {
        // An ordinary answer to OPEN refuses the socket with its status.
        return text(400, "a name is 1-32 characters\n");
    }
    let mut answer = ws::Answer::default();
    for event in events {
        match event {
            ws::Event::Open => {
                let joined = !present(now).await?.contains(&name);
                kv::set(&key, serde_json::to_vec(&Here { name: name.clone(), seen: now })?).await?;
                answer.accept().subscribe(CHANNEL).text(&json!({ "type": "here", "names": present(now).await? }).to_string());
                if joined {
                    ws::publish(CHANNEL, &json!({ "type": "join", "name": name }).to_string()).await?;
                }
            }
            ws::Event::Text(text) if text == "typing" || text == "ping" => {
                kv::set(&key, serde_json::to_vec(&Here { name: name.clone(), seen: now })?).await?;
                if text == "typing" {
                    ws::publish(CHANNEL, &json!({ "type": "typing", "name": name }).to_string()).await?;
                }
            }
            ws::Event::Close(_) | ws::Event::Disconnect => {
                kv::delete(&key).await?;
                // Someone with two tabs open is still here after closing one.
                if !present(now).await?.contains(&name) {
                    ws::publish(CHANNEL, &json!({ "type": "leave", "name": name }).to_string()).await?;
                }
            }
            _ => {}
        }
    }
    Ok(answer.into())
}

/// Everyone seen within `stale_ms`, by name. Sockets not seen since are
/// forgotten.
async fn present(now: u64) -> Result<BTreeSet<String>> {
    let stale: u64 = config::get("stale_ms").ok().flatten().and_then(|s| s.parse().ok()).unwrap_or(60_000);
    let mut names = BTreeSet::new();
    for key in kv::keys().await?.into_iter().filter(|k| k.starts_with("socket:")) {
        let Some(here) = kv::get(&key).await? else { continue };
        let here: Here = serde_json::from_slice(&here)?;
        if here.seen + stale < now {
            kv::delete(&key).await?;
        } else {
            names.insert(here.name);
        }
    }
    Ok(names)
}

/// A JSON answer.
fn json(status: u16, body: impl Into<Body>) -> Result {
    Ok(Response::builder().status(status).header(CONTENT_TYPE, "application/json").body(body.into())?)
}

/// A plain-text answer.
fn text(status: u16, body: impl Into<Body>) -> Result {
    Ok(Response::builder().status(status).header(CONTENT_TYPE, "text/plain; charset=utf-8").body(body.into())?)
}
