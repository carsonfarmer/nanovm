//! Notes with full-text search. Each person's notes are an object
//! (`/{user}/`) with its own SQLite database, and the notes table is an FTS5
//! index: saving a note indexes it in the same statement, and a search ranks
//! the matches (BM25) and marks where they matched.
//!
//! - `POST /{user}/notes` `{"title":"…","body":"…"}`: `201 {"id":n}`
//! - `GET`, `PUT`, `DELETE /{user}/notes/{id}`
//! - `GET /{user}/search?q=words`: the best 20 matches, each word a prefix

use std::cell::Cell;

use nanovm_sdk::http::header::CONTENT_TYPE;
use nanovm_sdk::http::{Request, Response};
use nanovm_sdk::{Body, RequestExt, Result, sqlite};
use serde::Deserialize;

nanovm_sdk::serve!(handle);

#[derive(Deserialize)]
struct Note {
    title: String,
    body: String,
}

async fn handle(req: Request<Body>) -> Result {
    schema().await?;
    let path: Vec<&str> = req.uri().path().trim_matches('/').split('/').collect();
    let note = || req.body().json::<Note>().ok().filter(|n| n.title.len() <= 200 && n.body.len() <= 64 << 10);
    // A note's id is its rowid in the index.
    let id = match path.as_slice() {
        ["notes", id] => match id.parse::<i64>() {
            Ok(id) => id,
            Err(_) => return text(404, "not found"),
        },
        _ => 0,
    };
    Ok(match (req.method().as_str(), path.as_slice()) {
        ("POST", ["notes"]) => {
            let Some(n) = note() else { return bad() };
            sqlite::execute("INSERT INTO notes (title, body) VALUES (?, ?)", &[&n.title, &n.body]).await?;
            json(201, format!(r#"{{"id":{}}}"#, sqlite::last_insert_rowid().await?))?
        }
        ("GET", ["notes", _]) => {
            let rows = sqlite::query("SELECT rowid AS id, title, body FROM notes WHERE rowid = ?", &[&id]).await?;
            match rows.json().as_array().and_then(|r| r.first().cloned()) {
                Some(note) => json(200, note.to_string())?,
                None => text(404, "not found")?,
            }
        }
        ("PUT", ["notes", _]) => {
            let Some(n) = note() else { return bad() };
            match sqlite::execute("UPDATE notes SET title = ?, body = ? WHERE rowid = ?", &[&n.title, &n.body, &id]).await? {
                0 => text(404, "not found")?,
                _ => Response::builder().status(204).body(Body::empty())?,
            }
        }
        ("DELETE", ["notes", _]) => match sqlite::execute("DELETE FROM notes WHERE rowid = ?", &[&id]).await? {
            0 => text(404, "not found")?,
            _ => Response::builder().status(204).body(Body::empty())?,
        },
        ("GET", ["search"]) => {
            let Some(query) = fts_query(&req.query("q").unwrap_or_default()) else {
                return text(400, "q: one to ten words\n");
            };
            let found = sqlite::query(
                "SELECT rowid AS id, title, snippet(notes, 1, '[', ']', '…', 12) AS snippet FROM notes \
                 WHERE notes MATCH ? ORDER BY bm25(notes, 5.0, 1.0) LIMIT 20",
                &[&query],
            )
            .await?;
            json(200, found.json().to_string())?
        }
        _ => text(404, "not found")?,
    })
}

/// The words of `q` as an FTS5 query: each one a quoted prefix, all of them
/// required. Quoting keeps a stray `"` or `NEAR(` from being query syntax.
fn fts_query(q: &str) -> Option<String> {
    let words: Vec<String> = q
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{w}\"*"))
        .collect();
    (1..=10).contains(&words.len()).then(|| words.join(" "))
}

fn bad() -> Result {
    text(400, "{\"title\": up to 200 bytes, \"body\": up to 64 KiB}\n")
}

thread_local! {
    static READY: Cell<bool> = const { Cell::new(false) };
}

async fn schema() -> Result<()> {
    if !READY.get() {
        // The table is the index: no separate content table to keep in step.
        sqlite::execute("CREATE VIRTUAL TABLE IF NOT EXISTS notes USING fts5 (title, body, tokenize = 'porter unicode61')", &[]).await?;
        READY.set(true);
    }
    Ok(())
}

/// A JSON answer.
fn json(status: u16, body: impl Into<Body>) -> Result {
    Ok(Response::builder().status(status).header(CONTENT_TYPE, "application/json").body(body.into())?)
}

/// A plain-text answer.
fn text(status: u16, body: impl Into<Body>) -> Result {
    Ok(Response::builder().status(status).header(CONTENT_TYPE, "text/plain; charset=utf-8").body(body.into())?)
}
