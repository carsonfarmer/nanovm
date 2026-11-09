//! Notes with full-text search. Each person's notes are an object
//! (`/{user}/`) with its own SQLite database, and the notes table is an FTS5
//! index: saving a note indexes it in the same statement, and a search ranks
//! the matches (BM25) and marks where they matched.
//!
//! - `POST /{user}/notes` `{"title":"…","body":"…"}`: `201 {"id":n}`
//! - `GET`, `PUT`, `DELETE /{user}/notes/{id}`
//! - `GET /{user}/search?q=words`: the best 20 matches, each word a prefix

use std::cell::Cell;

use nanovm_sdk::{Request, Response, Result, sqlite};
use serde::Deserialize;

nanovm_sdk::serve!(handle);

#[derive(Deserialize)]
struct Note {
    title: String,
    body: String,
}

async fn handle(req: Request) -> Result {
    schema().await?;
    let path: Vec<&str> = req.path().trim_matches('/').split('/').collect();
    let note = || req.json::<Note>().ok().filter(|n| n.title.len() <= 200 && n.body.len() <= 64 << 10);
    // A note's id is its rowid in the index.
    let id = match path.as_slice() {
        ["notes", id] => match id.parse::<i64>() {
            Ok(id) => id,
            Err(_) => return Ok(Response::not_found()),
        },
        _ => 0,
    };
    Ok(match (req.method(), path.as_slice()) {
        ("POST", ["notes"]) => {
            let Some(n) = note() else { return Ok(bad()) };
            sqlite::execute("INSERT INTO notes (title, body) VALUES (?, ?)", &[&n.title, &n.body]).await?;
            Response::json(format!(r#"{{"id":{}}}"#, sqlite::last_insert_rowid().await?)).status(201)
        }
        ("GET", ["notes", _]) => {
            let rows = sqlite::query("SELECT rowid AS id, title, body FROM notes WHERE rowid = ?", &[&id]).await?;
            match rows.json().as_array().and_then(|r| r.first().cloned()) {
                Some(note) => Response::json(note.to_string()),
                None => Response::not_found(),
            }
        }
        ("PUT", ["notes", _]) => {
            let Some(n) = note() else { return Ok(bad()) };
            match sqlite::execute("UPDATE notes SET title = ?, body = ? WHERE rowid = ?", &[&n.title, &n.body, &id]).await? {
                0 => Response::not_found(),
                _ => Response::new(204),
            }
        }
        ("DELETE", ["notes", _]) => match sqlite::execute("DELETE FROM notes WHERE rowid = ?", &[&id]).await? {
            0 => Response::not_found(),
            _ => Response::new(204),
        },
        ("GET", ["search"]) => {
            let Some(query) = fts_query(req.query("q").unwrap_or_default()) else {
                return Ok(Response::text("q: one to ten words\n").status(400));
            };
            let found = sqlite::query(
                "SELECT rowid AS id, title, snippet(notes, 1, '[', ']', '…', 12) AS snippet FROM notes \
                 WHERE notes MATCH ? ORDER BY bm25(notes, 5.0, 1.0) LIMIT 20",
                &[&query],
            )
            .await?;
            Response::json(found.json().to_string())
        }
        _ => Response::not_found(),
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

fn bad() -> Response {
    Response::text("{\"title\": up to 200 bytes, \"body\": up to 64 KiB}\n").status(400)
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
