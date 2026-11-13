//! Offline-first todo sync. Each person's list is an object (`/{user}/`).
//! Their devices work offline, queue what they change as operations, and
//! sync when they can: one request sends the queue and gets back everything
//! that changed since the device last synced.
//!
//! - Every applied operation bumps the list's version; each field of a todo
//!   remembers the version that last wrote it.
//! - An operation says which version of the todo its device last saw
//!   (`base`). A field changed since then, by another device, is a conflict:
//!   the list keeps what it has and says so, and the device shows it.
//!   Changes to different fields of the same todo merge.
//! - Operations have ids: a sync retried after a lost answer applies nothing
//!   twice.
//!
//! - `POST /{user}/sync` `{"since":n,"ops":[{"id":"…","todo":"…","base":n,
//!   "title":"…"?,"done":bool?,"delete":true?}]}`: `{"version","changes","conflicts"}`

use std::collections::BTreeMap;

use nanovm_sdk::http::header::CONTENT_TYPE;
use nanovm_sdk::http::{Request, Response};
use nanovm_sdk::{Body, Result, kv, serde_json};
use serde::{Deserialize, Serialize};

nanovm_sdk::serve!(handle);

#[derive(Serialize, Deserialize, Default, Clone)]
struct Todo {
    id: String,
    title: String,
    done: bool,
    deleted: bool,
    /// The list's version when this todo last changed, and when each field did.
    version: u64,
    #[serde(default)]
    written: BTreeMap<String, u64>,
}

#[derive(Deserialize)]
struct Sync {
    since: u64,
    #[serde(default)]
    ops: Vec<Op>,
}

#[derive(Deserialize)]
struct Op {
    id: String,
    todo: String,
    base: u64,
    title: Option<String>,
    done: Option<bool>,
    #[serde(default)]
    delete: bool,
}

async fn handle(req: Request<Body>) -> Result {
    if (req.method().as_str(), req.uri().path()) != ("POST", "/sync") {
        return text(404, "not found");
    }
    let ok = |s: &Sync| s.ops.len() <= 100 && s.ops.iter().all(|o| valid(&o.id) && valid(&o.todo));
    let Some(sync) = req.body().json::<Sync>().ok().filter(ok) else {
        return text(400, "{\"since\", \"ops\": up to 100 with ids of 1-64 of [A-Za-z0-9_-]}\n");
    };
    let mut version = kv::get_text("version").await?.and_then(|v| v.parse().ok()).unwrap_or(0u64);
    let mut conflicts = vec![];
    for op in sync.ops {
        let seen = format!("op:{}", op.id);
        if kv::exists(&seen).await? {
            continue;
        }
        kv::set(&seen, "").await?;
        let key = format!("todo:{}", op.todo);
        let mut todo: Todo = match kv::get(&key).await? {
            Some(json) => serde_json::from_slice(&json)?,
            None => Todo { id: op.todo.clone(), ..Default::default() },
        };
        version += 1;
        // A field written since this device last saw the todo isn't theirs to overwrite.
        let mut clash = vec![];
        let mut write = |field: &str, todo: &mut Todo| {
            if todo.written.get(field).is_some_and(|&v| v > op.base) || (todo.deleted && field != "deleted") {
                clash.push(field.to_string());
                false
            } else {
                todo.written.insert(field.into(), version);
                true
            }
        };
        if let Some(title) = op.title.as_ref().filter(|t| t.len() <= 500) {
            if write("title", &mut todo) {
                todo.title = title.clone();
            }
        }
        if let Some(done) = op.done {
            if write("done", &mut todo) {
                todo.done = done;
            }
        }
        if op.delete && write("deleted", &mut todo) {
            todo.deleted = true;
        }
        if !clash.is_empty() {
            conflicts.push(serde_json::json!({ "op": op.id, "todo": todo.id, "fields": clash }));
        }
        todo.version = version;
        kv::set(&key, serde_json::to_vec(&todo)?).await?;
    }
    kv::set("version", version.to_string()).await?;

    let mut changes = vec![];
    for key in kv::keys().await?.into_iter().filter(|k| k.starts_with("todo:")) {
        let todo: Todo = serde_json::from_slice(&kv::get(&key).await?.unwrap_or_default())?;
        if todo.version > sync.since {
            changes.push(todo);
        }
    }
    changes.sort_by_key(|t| t.version);
    let out = serde_json::json!({ "version": version, "changes": changes, "conflicts": conflicts });
    json(200, out.to_string())
}

fn valid(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}

/// A JSON answer.
fn json(status: u16, body: impl Into<Body>) -> Result {
    Ok(Response::builder().status(status).header(CONTENT_TYPE, "application/json").body(body.into())?)
}

/// A plain-text answer.
fn text(status: u16, body: impl Into<Body>) -> Result {
    Ok(Response::builder().status(status).header(CONTENT_TYPE, "text/plain; charset=utf-8").body(body.into())?)
}
