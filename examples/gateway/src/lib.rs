//! An LLM gateway: per-user daily token quotas in front of a streaming
//! completion API. Each user is an object (`/{user}/`), so their quota is
//! checked and spent one request at a time. A completion is streamed from
//! the upstream API to the client as it is generated (Server-Sent Events,
//! one token per event), and counted on the way through.
//!
//! The quota is charged *before* streaming and refunded after: a request
//! reserves `max_tokens` with its answer (that commits at once), and the
//! stream, once done, gives back what it didn't use. A stream that is cut
//! off (by the client, the upstream, or `post_return_timeout_ms`, which
//! rolls back what it changed) keeps the whole reservation, so a broken
//! stream is never free.
//!
//! - `POST /{user}/complete` `{"prompt":"…"}`: a `text/event-stream` of the
//!   upstream's tokens, `429` over quota
//! - `GET /{user}/usage`: `{"used":n,"limit":n,"day":"…"}`

use std::time::{SystemTime, UNIX_EPOCH};

use nanovm_sdk::{Request, Response, Result, config, fetch, kv, serde_json};
use serde_json::json;

nanovm_sdk::serve!(handle);

async fn handle(req: Request) -> Result {
    let day = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() / 86_400;
    let used_key = format!("used:{day}");
    let used = kv::get_text(&used_key).await?.and_then(|n| n.parse().ok()).unwrap_or(0i64);
    let (limit, max) = (setting("tokens_per_day", 2000), setting("max_tokens", 200));
    Ok(match (req.method(), req.path()) {
        ("GET", "/usage") => Response::json(json!({ "used": used, "limit": limit, "day": day }).to_string()),
        ("POST", "/complete") => {
            let Some(prompt) = req.json::<serde_json::Value>().ok().and_then(|p| p["prompt"].as_str().map(String::from)) else {
                return Ok(Response::text("{\"prompt\": \"…\"}\n").status(400));
            };
            if used + max > limit {
                return Ok(Response::json(json!({ "error": "quota", "used": used, "limit": limit }).to_string()).status(429));
            }
            let url = config::get("upstream").ok().flatten().unwrap_or_default();
            let mut upstream = fetch(url)
                .method("POST")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", nanovm_sdk::env("UPSTREAM_KEY")))
                .body(json!({ "prompt": prompt, "max_tokens": max, "stream": true }).to_string())
                .stream()
                .await?;
            if upstream.status != 200 {
                return Ok(Response::text(format!("upstream answered {}\n", upstream.status)).status(502));
            }
            // Reserved now: this commits with the answer, before any token flows.
            kv::set(&used_key, (used + max).to_string()).await?;
            Response::stream("text/event-stream", move |mut body| async move {
                let mut tokens = 0;
                while let Some(piece) = upstream.next().await {
                    tokens += String::from_utf8_lossy(&piece).matches("data: {").count() as i64;
                    if !body.write(piece).await {
                        return; // The client left: the reservation stands.
                    }
                }
                // Done: give back what wasn't used. This commits when the
                // stream ends, or never if it runs out of time.
                let _ = kv::increment(&used_key, tokens.min(max) - max).await;
            })
            .header("cache-control", "no-cache")
        }
        _ => Response::not_found(),
    })
}

fn setting(key: &str, default: i64) -> i64 {
    config::get(key).ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(default)
}
