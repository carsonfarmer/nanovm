//! An LLM gateway: per-user daily token quotas in front of a streaming
//! completion API. Each user is an object (`/{user}/`), so their quota is
//! checked and spent one request at a time. A completion is streamed from
//! the upstream API to the client as it is generated (Server-Sent Events,
//! one token per event, `[DONE]` at the end), and counted on the way
//! through: its events are parsed as they arrive, however the reads split
//! them.
//!
//! The quota is charged *before* streaming and refunded after: a request
//! reserves `max_tokens` with its answer (that commits at once), and the
//! stream, once done, gives back what it didn't use. Done means verified:
//! the upstream said `[DONE]` and its body arrived whole. What was used is
//! the upstream's own count if it reports one (`usage.completion_tokens`,
//! as OpenAI's API does), else the token events seen. A stream that is cut
//! off (by the client, the upstream, or `post_return_timeout_ms`, which
//! rolls back what it changed) keeps the whole reservation, so a broken
//! stream is never free.
//!
//! - `POST /{user}/complete` `{"prompt":"…"}`: a `text/event-stream` of the
//!   upstream's tokens, `429` over quota
//! - `GET /{user}/usage`: `{"used":n,"limit":n,"day":"…"}`

use std::time::{SystemTime, UNIX_EPOCH};

use nanovm_sdk::http::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE};
use nanovm_sdk::http::{Request, Response};
use nanovm_sdk::{Body, Result, config, fetch_streaming, kv, serde_json};
use serde_json::json;

nanovm_sdk::serve!(handle);

async fn handle(req: Request<Body>) -> Result {
    let day = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() / 86_400;
    let used_key = format!("used:{day}");
    let used = kv::get_text(&used_key).await?.and_then(|n| n.parse().ok()).unwrap_or(0i64);
    let (limit, max) = (setting("tokens_per_day", 2000), setting("max_tokens", 200));
    Ok(match (req.method().as_str(), req.uri().path()) {
        ("GET", "/usage") => json(200, json!({ "used": used, "limit": limit, "day": day }).to_string())?,
        ("POST", "/complete") => {
            let Some(prompt) = req.body().json::<serde_json::Value>().ok().and_then(|p| p["prompt"].as_str().map(String::from)) else {
                return text(400, "{\"prompt\": \"…\"}\n");
            };
            if used + max > limit {
                return json(429, json!({ "error": "quota", "used": used, "limit": limit }).to_string());
            }
            let url = config::get("upstream").ok().flatten().unwrap_or_default();
            let request = Request::post(url)
                .header(CONTENT_TYPE, "application/json")
                .header(AUTHORIZATION, format!("Bearer {}", nanovm_sdk::env("UPSTREAM_KEY")))
                .body(json!({ "prompt": prompt, "max_tokens": max, "stream": true }).to_string())?;
            let (upstream, mut tokens) = fetch_streaming(request).await?.into_parts();
            if upstream.status != 200 {
                return text(502, format!("upstream answered {}\n", upstream.status.as_u16()));
            }
            // Reserved now: this commits with the answer, before any token flows.
            kv::set(&used_key, (used + max).to_string()).await?;
            let stream = Body::stream(move |mut body| async move {
                let mut events = Events::default();
                // The upstream breaking off breaks the answer off too, so the
                // client can tell; the reservation stands.
                while let Some(piece) = tokens.next().await? {
                    events.read(&piece);
                    if !body.write(piece).await {
                        return Ok(()); // The client left: the reservation stands.
                    }
                }
                // Done, and verifiably so: give back what wasn't used. This
                // commits when the stream ends, or never if it runs out of
                // time. Without `[DONE]` the reservation stands.
                if let Some(used) = events.used() {
                    let _ = kv::increment(&used_key, used.clamp(0, max) - max).await;
                }
                Ok(())
            });
            Response::builder().header(CONTENT_TYPE, "text/event-stream").header(CACHE_CONTROL, "no-cache").body(stream)?
        }
        _ => text(404, "not found")?,
    })
}

/// The upstream's Server-Sent Events, read as its body arrives: an event
/// ends at a blank line, wherever the reads split it, and lines end with
/// LF, CRLF or CR (a CRLF split between reads included).
#[derive(Default)]
struct Events {
    line: Vec<u8>,
    after_cr: bool,
    /// The `data` of the event so far.
    data: Option<String>,
    tokens: i64,
    usage: Option<i64>,
    done: bool,
}

impl Events {
    fn read(&mut self, bytes: &[u8]) {
        for &b in bytes {
            let after_cr = std::mem::replace(&mut self.after_cr, b == b'\r');
            match b {
                b'\n' if after_cr => {}
                b'\n' | b'\r' => {
                    let line = std::mem::take(&mut self.line);
                    self.line(&String::from_utf8_lossy(&line));
                }
                b => self.line.push(b),
            }
        }
    }

    fn line(&mut self, line: &str) {
        if line.is_empty() {
            match self.data.take().as_deref() {
                Some("[DONE]") => self.done = true,
                Some(data) => {
                    self.tokens += 1;
                    let event: serde_json::Value = serde_json::from_str(data).unwrap_or_default();
                    if let Some(n) = event["usage"]["completion_tokens"].as_i64() {
                        self.usage = Some(n);
                    }
                }
                None => {}
            }
        } else if let Some(value) = line.strip_prefix("data:").or((line == "data").then_some("")) {
            let value = value.strip_prefix(' ').unwrap_or(value);
            match &mut self.data {
                Some(data) => {
                    data.push('\n');
                    data.push_str(value);
                }
                None => self.data = Some(value.into()),
            }
        }
    }

    /// The tokens a finished stream used: none if it didn't say `[DONE]`.
    fn used(&self) -> Option<i64> {
        self.done.then(|| self.usage.unwrap_or(self.tokens))
    }
}

fn setting(key: &str, default: i64) -> i64 {
    config::get(key).ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// A JSON answer.
fn json(status: u16, body: impl Into<Body>) -> Result {
    Ok(Response::builder().status(status).header(CONTENT_TYPE, "application/json").body(body.into())?)
}

/// A plain-text answer.
fn text(status: u16, body: impl Into<Body>) -> Result {
    Ok(Response::builder().status(status).header(CONTENT_TYPE, "text/plain; charset=utf-8").body(body.into())?)
}
