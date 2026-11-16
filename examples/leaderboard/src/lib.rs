//! Leaderboards on SQLite. Each board is an object (`/{board}/`) with its
//! own database: one row per player with their best score, and an index
//! that serves the top of the board, a page at a time, and any player's rank.
//!
//! - `POST /{board}/scores` `{"player":"ann","score":120}` (or an array of up
//!   to 100): keeps each player's best; answers with their best and rank
//! - `GET /{board}/top?limit=10&offset=0`: a page of the board, best first
//! - `GET /{board}/players/{player}`: a player's best and rank, or `404`
//! - `DELETE /{board}/players/{player}`

use std::cell::Cell;
use std::time::{SystemTime, UNIX_EPOCH};

use nanovm_sdk::http::header::CONTENT_TYPE;
use nanovm_sdk::http::{Request, Response};
use nanovm_sdk::{Body, RequestExt, Result, serde_json, sqlite};
use serde::{Deserialize, Serialize};

nanovm_sdk::serve!(handle);

#[derive(Deserialize)]
#[serde(untagged)]
enum Scores {
    One(Score),
    Many(Vec<Score>),
}

#[derive(Serialize, Deserialize)]
struct Score {
    player: String,
    score: i64,
}

// Ties go to whoever got there first, then by name.
const ORDER: &str = "ORDER BY score DESC, at, player";

async fn handle(req: Request<Body>) -> Result {
    schema().await?;
    let player = req.uri().path().strip_prefix("/players/").map(|p| p.to_string());
    Ok(match (req.method().as_str(), req.uri().path(), player) {
        ("POST", "/scores", _) => {
            let scores = match req.body().json::<Scores>() {
                Ok(Scores::One(s)) => vec![s],
                Ok(Scores::Many(s)) if s.len() <= 100 => s,
                _ => return bad(),
            };
            if !scores.iter().all(|s| (1..=64).contains(&s.player.chars().count())) {
                return bad();
            }
            let at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64;
            let batch = serde_json::to_string(&scores)?;
            // One statement for the whole batch: every statement that writes
            // is a SQLite transaction of its own, with its own syncs.
            sqlite::execute(
                "INSERT INTO scores SELECT value ->> 'player', value ->> 'score', ? FROM json_each(?) WHERE true \
                 ON CONFLICT (player) DO UPDATE SET score = excluded.score, at = excluded.at \
                 WHERE excluded.score > scores.score",
                &[&at, &batch],
            )
            .await?;
            json(200, standings(&batch).await?.json().to_string())?
        }
        ("GET", "/top", _) => {
            let (limit, offset) = (req.query_or("limit", 10u32).min(100), req.query_or("offset", 0u32));
            let page = sqlite::query(
                &format!("SELECT rank() OVER (ORDER BY score DESC) AS rank, player, score FROM scores {ORDER} LIMIT ? OFFSET ?"),
                &[&limit, &offset],
            )
            .await?;
            json(200, page.json().to_string())?
        }
        ("GET", _, Some(player)) => match standings(&serde_json::json!([{ "player": player }]).to_string()).await?.json() {
            serde_json::Value::Array(mut one) if !one.is_empty() => json(200, one.remove(0).to_string())?,
            _ => text(404, "not found")?,
        },
        ("DELETE", _, Some(player)) => {
            sqlite::execute("DELETE FROM scores WHERE player = ?", &[&player]).await?;
            Response::builder().status(204).body(Body::empty())?
        }
        _ => text(404, "not found")?,
    })
}

/// The best and rank of the players in `players`, a JSON array of objects
/// with a `player`. A rank is one more than the number of better scores.
async fn standings(players: &str) -> Result<sqlite::Rows> {
    Ok(sqlite::query(
        "SELECT player, score, (SELECT count(*) FROM scores b WHERE b.score > s.score) + 1 AS rank \
         FROM scores s WHERE player IN (SELECT value ->> 'player' FROM json_each(?)) ORDER BY rank, player",
        &[&players],
    )
    .await?)
}

fn bad() -> Result {
    text(400, "a score is {\"player\": 1-64 characters, \"score\": an integer}, up to 100 at once\n")
}

thread_local! {
    static READY: Cell<bool> = const { Cell::new(false) };
}

/// Creates the table and index, once per instance.
async fn schema() -> Result<()> {
    if !READY.get() {
        sqlite::execute("CREATE TABLE IF NOT EXISTS scores (player TEXT PRIMARY KEY, score INTEGER NOT NULL, at INTEGER NOT NULL)", &[]).await?;
        sqlite::execute("CREATE INDEX IF NOT EXISTS by_score ON scores (score DESC, at, player)", &[]).await?;
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
