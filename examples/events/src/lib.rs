//! Server-Sent Events from an object, and what a streamed response costs.
//! Each feed is an object (`/{feed}/`) with an append-only log of events.
//!
//! - `POST /{feed}/events` (body: the event's data): appends it; its id
//! - `GET /{feed}/events` (`Last-Event-ID` or `?after=N`): an event stream of
//!   every event after that one, then `retry: 1000`, then it ends; a browser's
//!   `EventSource` reconnects a second later with the last id it saw
//! - `GET /{feed}/countdown?from=5&every_ms=200`: a stream that counts down,
//!   an event every `every_ms`, then `done`; a finished countdown is counted
//! - `GET /{feed}/`: `{"events":n,"countdowns":n}`
//!
//! A streamed body is written after the handler returns, and the object
//! serves nothing else until it is done, within `post_return_timeout_ms`. So
//! a feed never holds a stream open waiting for new events (no one could
//! post them): it sends what it has and lets the client come back.

use std::time::Duration;

use nanovm_sdk::{Request, Response, Result, data, kv, serde_json, sse};

nanovm_sdk::serve!(handle);

const LOG: &str = "events.log";

async fn handle(req: Request) -> Result {
    Ok(match (req.method(), req.path()) {
        ("POST", "/events") => {
            let text = req.text()?;
            if text.len() > 4096 {
                return Ok(Response::text("an event is at most 4 KiB\n").status(400));
            }
            let id = kv::increment("events", 1).await?;
            // One line per event: its id, a space, its data as a JSON string.
            data().append(LOG, format!("{id} {}\n", serde_json::to_string(text)?)).await?;
            Response::json(format!(r#"{{"id":{id}}}"#)).status(201)
        }
        ("GET", "/events") => {
            let after: u64 = req.header("last-event-id").or(req.query("after")).and_then(|i| i.parse().ok()).unwrap_or(0);
            let log = data().read_text(LOG).await?.unwrap_or_default();
            let mut out = String::from("retry: 1000\n\n");
            for (id, text) in log.lines().filter_map(|l| l.split_once(' ')) {
                if id.parse::<u64>().is_ok_and(|id| id > after) {
                    out.push_str(&sse(Some(id), None, &serde_json::from_str::<String>(text)?));
                }
            }
            // Everything there is, at once: no stream needs to stay open.
            Response::bytes("text/event-stream", out).header("cache-control", "no-cache")
        }
        ("GET", "/countdown") => {
            let from = req.query_or("from", 5u32).min(100);
            let every = Duration::from_millis(req.query_or("every_ms", 200u64).min(10_000));
            Response::stream("text/event-stream", move |mut body| async move {
                for n in (1..=from).rev() {
                    if !body.write(sse(None, Some("tick"), &n.to_string())).await {
                        return;
                    }
                    nanovm_sdk::sleep(every).await;
                }
                body.write(sse(None, Some("done"), "liftoff")).await;
                // Changed after answering: this commits when the stream ends,
                // or rolls back if the stream outlives post_return_timeout_ms.
                let _ = kv::increment("countdowns", 1).await;
            })
            .header("cache-control", "no-cache")
        }
        ("GET", "/") => {
            let get = async |k| kv::get_text(k).await.map(|v| v.unwrap_or("0".into()));
            Response::json(format!(r#"{{"events":{},"countdowns":{}}}"#, get("events").await?, get("countdowns").await?))
        }
        _ => Response::not_found(),
    })
}
