//! Live multi-room chat. Each room is an object, `/{room}/…`: its history is
//! a file in its volume, so it survives eviction and restarts, and its
//! members are WebSockets subscribed to it. The `lobby` object indexes the
//! rooms; each room tells it of every message with a call between objects,
//! and it tells its own sockets. The component serves its own page.
//!
//! - `GET /{room}/`: the page (`/lobby/` lists the rooms)
//! - `GET /{room}/messages`: the room's last 100 messages, oldest first
//! - `POST /{room}/messages?name=…`: posts the body as a message
//! - `/{room}/ws?name=…`: a WebSocket that gets the history, then every new
//!   message; each text frame it sends is a message
//! - `GET /lobby/rooms`: every room's message count and last message

use std::time::{SystemTime, UNIX_EPOCH};

use nanovm_sdk::http::header::CONTENT_TYPE;
use nanovm_sdk::http::{Request, Response};
use nanovm_sdk::{Body, RequestExt, Result, data, kv, object, serde_json, ws};
use serde::{Deserialize, Serialize};

nanovm_sdk::serve!(handle);

const LOBBY: &str = "lobby";
const LOG: &str = "messages.jsonl";
const KEEP: usize = 100;
const BAD: &str = "a message is 1-2000 characters, from a name of 1-32";

#[derive(Serialize, Deserialize)]
struct Message {
    name: String,
    text: String,
    at: u64,
}

/// What the lobby knows of a room.
#[derive(Serialize, Deserialize)]
struct Room {
    #[serde(default)]
    room: String,
    messages: u64,
    last: Message,
}

async fn handle(req: Request<Body>) -> Result {
    let lobby = nanovm_sdk::id() == LOBBY;
    if let Some(events) = ws::events(&req) {
        return socket(&req, events, lobby).await;
    }
    Ok(match (req.method().as_str(), req.uri().path()) {
        ("GET", "/") => html(200, include_str!("index.html"))?,
        ("GET", "/messages") if !lobby => json(200, format!("[{}]", history().await?.join(",")))?,
        ("POST", "/messages") if !lobby => match post(&req.query("name").unwrap_or_default(), req.body().text()?).await? {
            Some(message) => json(200, message)?,
            None => text(400, BAD)?,
        },
        ("GET", "/rooms") if lobby => json(200, format!("[{}]", rooms().await?.join(",")))?,
        ("POST", p) if lobby && p.starts_with("/rooms/") => record(&p[7..], &req).await?,
        _ => text(404, "not found")?,
    })
}

/// A room's last messages, oldest first, as JSON.
async fn history() -> Result<Vec<String>> {
    let log = data().read_text(LOG).await?.unwrap_or_default();
    let lines: Vec<&str> = log.lines().collect();
    Ok(lines[lines.len().saturating_sub(KEEP)..].iter().map(|l| l.to_string()).collect())
}

/// Stores, sends and indexes a message; `None` if it isn't one.
async fn post(name: &str, text: &str) -> Result<Option<String>> {
    let (name, text) = (name.trim(), text.trim());
    if !(1..=32).contains(&name.chars().count()) || !(1..=2000).contains(&text.chars().count()) {
        return Ok(None);
    }
    let at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    let last = Message { name: name.into(), text: text.into(), at };
    let message = serde_json::to_string(&last)?;
    data().append(LOG, format!("{message}\n")).await?;
    let messages = kv::increment("count", 1).await? as u64;
    if messages % KEEP as u64 == 0 {
        // Keeps the log to at most twice the history.
        let kept: Vec<String> = history().await?.into_iter().map(|m| m + "\n").collect();
        data().write(LOG, kept.concat()).await?;
    }
    ws::publish("chat", &message).await?;
    // The index is a summary: if this call fails, the next message corrects it.
    let room = serde_json::to_string(&Room { room: nanovm_sdk::id(), messages, last })?;
    let _ = object(LOBBY).post(&format!("/rooms/{}", nanovm_sdk::id()), room).await;
    Ok(Some(message))
}

async fn socket(req: &Request<Body>, events: Vec<ws::Event>, lobby: bool) -> Result {
    let mut answer = ws::Answer::default();
    for event in events {
        match event {
            ws::Event::Open => {
                answer.accept().subscribe(if lobby { "rooms" } else { "chat" });
                for json in if lobby { rooms().await? } else { history().await? } {
                    answer.text(&json);
                }
            }
            ws::Event::Text(text) if !lobby => {
                if post(&req.query("name").unwrap_or_default(), &text).await?.is_none() {
                    answer.text(&format!(r#"{{"error":"{BAD}"}}"#));
                }
            }
            _ => {}
        }
    }
    Ok(answer.into())
}

/// The lobby records a room's latest state. Anyone can post here, not only
/// rooms, so what is stored is parsed and written afresh, never kept as sent.
async fn record(room: &str, req: &Request<Body>) -> Result {
    // An object id, as the node allows them.
    let named = (1..=128).contains(&room.len()) && !room.starts_with(['.', '_']);
    let named = named && room.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b));
    let (true, Ok(mut entry)) = (named, req.body().json::<Room>()) else {
        return text(400, "bad room or entry");
    };
    entry.room = room.into();
    let entry = serde_json::to_string(&entry)?;
    kv::set(room, entry.as_str()).await?;
    ws::publish("rooms", &entry).await?;
    json(200, entry)
}

/// Every room the lobby knows, as JSON.
async fn rooms() -> Result<Vec<String>> {
    let mut rooms = vec![];
    for room in kv::keys().await? {
        rooms.extend(kv::get_text(&room).await?);
    }
    Ok(rooms)
}

/// A JSON answer.
fn json(status: u16, body: impl Into<Body>) -> Result {
    Ok(Response::builder().status(status).header(CONTENT_TYPE, "application/json").body(body.into())?)
}

/// A plain-text answer.
fn text(status: u16, body: impl Into<Body>) -> Result {
    Ok(Response::builder().status(status).header(CONTENT_TYPE, "text/plain; charset=utf-8").body(body.into())?)
}

/// An HTML answer.
fn html(status: u16, body: impl Into<Body>) -> Result {
    Ok(Response::builder().status(status).header(CONTENT_TYPE, "text/html; charset=utf-8").body(body.into())?)
}
