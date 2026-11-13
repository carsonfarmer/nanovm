//! Reminders, without alarms. nanovm runs code only when a request comes,
//! so nothing can wake an object at a time it chose. Two patterns work
//! today, and this app has both:
//!
//! - **An external clock.** Something outside (cron, a CI schedule, an
//!   uptime checker) sends `POST /clock/tick` every minute. The object
//!   `clock` keeps every reminder's due time in order, and on each tick calls
//!   the owner's object for each one that is due; that object pushes it to
//!   its owner's WebSockets. A reminder is late by up to the tick interval.
//! - **Client polling.** A client asks `GET /{user}/due` when it wakes up or
//!   every so often, and gets what is due. No clock is needed, and nothing is
//!   delivered while nobody asks.
//!
//! Either way a reminder is delivered once: whichever comes first marks it
//! fired, in its owner's object, one request at a time.
//!
//! - `POST /{user}/reminders` `{"text":"…","in_ms":60000}` (or `"at"`, ms since the epoch)
//! - `GET /{user}/reminders`, `GET /{user}/due`, `/{user}/ws`
//! - `POST /clock/tick`: `{"fired":n}`

use std::time::{SystemTime, UNIX_EPOCH};

use nanovm_sdk::http::header::CONTENT_TYPE;
use nanovm_sdk::http::{Request, Response};
use nanovm_sdk::{Body, RequestExt, Result, kv, object, serde_json, ws};
use serde::{Deserialize, Serialize};
use serde_json::json;

nanovm_sdk::serve!(handle);

#[derive(Serialize, Deserialize)]
struct Reminder {
    id: i64,
    text: String,
    at: u64,
    fired: bool,
}

#[derive(Deserialize)]
struct New {
    text: String,
    at: Option<u64>,
    in_ms: Option<u64>,
}

async fn handle(req: Request<Body>) -> Result {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    if nanovm_sdk::id() == "clock" {
        return clock(&req, now).await;
    }
    if let Some(events) = ws::events(&req) {
        let mut answer = ws::Answer::default();
        if events.contains(&ws::Event::Open) {
            answer.accept().subscribe("reminders");
        }
        return Ok(answer.into());
    }
    Ok(match (req.method().as_str(), req.uri().path()) {
        ("POST", "/reminders") => {
            let new = req.body().json::<New>().ok().filter(|n| (1..=500).contains(&n.text.len()));
            let Some(New { text, at, in_ms }) = new else {
                return text(400, "{\"text\": 1-500 bytes, \"at\" or \"in_ms\"}\n");
            };
            let at = at.unwrap_or(now + in_ms.unwrap_or(0));
            let id = kv::increment("next", 1).await?;
            let reminder = Reminder { id, text, at, fired: false };
            kv::set(&format!("r:{id:010}"), serde_json::to_vec(&reminder)?).await?;
            // The clock commits this on its own, before we do; if we then fail,
            // it calls about a reminder we don't have, and is told 404.
            let due = json!({ "user": nanovm_sdk::id(), "id": id, "at": at }).to_string();
            let reply = object("clock").post("/schedule", due).await?;
            if reply.status() != 204 {
                return Err(nanovm_sdk::Error::new(format!("the clock said {}", reply.status())));
            }
            json(201, serde_json::to_string(&reminder)?)?
        }
        ("GET", "/reminders") => json(200, serde_json::to_string(&all().await?)?)?,
        ("GET", "/due") => {
            let mut due = vec![];
            for r in all().await?.into_iter().filter(|r| !r.fired && r.at <= now) {
                due.push(fire(r).await?);
            }
            json(200, serde_json::to_string(&due)?)?
        }
        // For the clock (anyone may call it: firing twice is firing once).
        ("POST", "/fire") => {
            let key = format!("r:{:010}", req.query_or("id", 0i64));
            match kv::get(&key).await? {
                Some(r) => {
                    let r: Reminder = serde_json::from_slice(&r)?;
                    if r.fired || r.at > now {
                        return Ok(Response::builder().status(204).body(Body::empty())?);
                    }
                    json(200, serde_json::to_string(&fire(r).await?)?)?
                }
                None => text(404, "not found")?,
            }
        }
        _ => text(404, "not found")?,
    })
}

/// Marks `r` fired and pushes it to its owner's sockets.
async fn fire(mut r: Reminder) -> Result<Reminder> {
    r.fired = true;
    let json = serde_json::to_string(&r)?;
    kv::set(&format!("r:{:010}", r.id), json.as_str()).await?;
    ws::publish("reminders", &json).await?;
    Ok(r)
}

async fn all() -> Result<Vec<Reminder>> {
    let mut keys: Vec<String> = kv::keys().await?.into_iter().filter(|k| k.starts_with("r:")).collect();
    keys.sort();
    let mut out = vec![];
    for key in keys {
        if let Some(json) = kv::get(&key).await? {
            out.push(serde_json::from_slice(&json)?);
        }
    }
    Ok(out)
}

/// The clock: keys `due:{at}:{user}:{id}`, zero-padded so they sort by time.
async fn clock(req: &Request<Body>, now: u64) -> Result {
    Ok(match (req.method().as_str(), req.uri().path()) {
        ("POST", "/schedule") => {
            let due: serde_json::Value = req.body().json()?;
            let (Some(user), Some(id), Some(at)) = (due["user"].as_str(), due["id"].as_i64(), due["at"].as_u64()) else {
                return Ok(Response::builder().status(400).body(Body::empty())?);
            };
            kv::set(&format!("due:{at:015}:{user}:{id}"), "").await?;
            Response::builder().status(204).body(Body::empty())?
        }
        ("POST", "/tick") => {
            let mut due: Vec<String> = kv::keys().await?.into_iter().filter(|k| k.starts_with("due:")).collect();
            due.sort();
            let mut fired = 0;
            for key in due {
                let mut parts = key.splitn(4, ':').skip(1);
                let (Some(at), Some(user), Some(id)) = (parts.next(), parts.next(), parts.next()) else { continue };
                if at.parse::<u64>().unwrap_or(u64::MAX) > now {
                    break;
                }
                // Each call commits in the owner's object before it answers;
                // one that fails stays due for the next tick.
                let reply = object(user).post(&format!("/fire?id={id}"), "").await?;
                // 200: fired now; 204: already fired (polled); 404: never made.
                if matches!(reply.status().as_u16(), 200 | 204 | 404) {
                    kv::delete(&key).await?;
                    fired += i32::from(reply.status() == 200);
                }
            }
            json(200, json!({ "fired": fired }).to_string())?
        }
        _ => text(404, "not found")?,
    })
}

/// A JSON answer.
fn json(status: u16, body: impl Into<Body>) -> Result {
    Ok(Response::builder().status(status).header(CONTENT_TYPE, "application/json").body(body.into())?)
}

/// A plain-text answer.
fn text(status: u16, body: impl Into<Body>) -> Result {
    Ok(Response::builder().status(status).header(CONTENT_TYPE, "text/plain; charset=utf-8").body(body.into())?)
}
