//! An analytics collector. Each site is an object (`/{site}/`) with its
//! own SQLite database: raw events, and a per-day rollup that a trigger
//! keeps as events arrive, so reading a month of counts never scans a
//! month of events. Batches are one statement each.
//!
//! - `POST /{site}/events` `{"name":"view","path":"/","user":"u1"}`, or an
//!   array of up to 100; `at` (ms since the epoch, within the last 30 days)
//!   for events sent late
//! - `GET /{site}/days?from=YYYY-MM-DD&to=YYYY-MM-DD`: per day and event,
//!   how many and from how many users
//! - `GET /{site}/days.csv?from=…&to=…`: the same as CSV

use std::cell::Cell;
use std::time::{SystemTime, UNIX_EPOCH};

use nanovm_sdk::{Request, Response, Result, serde_json, sqlite};
use serde::{Deserialize, Serialize};

nanovm_sdk::serve!(handle);

#[derive(Serialize, Deserialize)]
struct Event {
    name: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    user: String,
    at: Option<i64>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Batch {
    One(Event),
    Many(Vec<Event>),
}

const DAY_MS: i64 = 86_400_000;

async fn handle(req: Request) -> Result {
    schema().await?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64;
    Ok(match (req.method(), req.path()) {
        ("POST", "/events") => {
            let events = match req.json::<Batch>() {
                Ok(Batch::One(e)) => vec![e],
                Ok(Batch::Many(e)) if e.len() <= 100 => e,
                _ => return Ok(bad()),
            };
            let ok = |e: &Event| {
                let name = (1..=64).contains(&e.name.len()) && e.name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b));
                let at = e.at.is_none_or(|at| (now - 30 * DAY_MS..=now + 300_000).contains(&at));
                name && at && e.path.len() <= 512 && e.user.len() <= 128
            };
            if !events.iter().all(ok) {
                return Ok(bad());
            }
            // One statement for the batch; the trigger counts each event.
            let n = sqlite::execute(
                "INSERT INTO events (at, day, name, path, user) \
                 SELECT t, date(t / 1000, 'unixepoch'), name, path, user FROM \
                 (SELECT coalesce(value ->> 'at', ?1) AS t, value ->> 'name' AS name, value ->> 'path' AS path, \
                         value ->> 'user' AS user FROM json_each(?2))",
                &[&now, &serde_json::to_string(&events)?],
            )
            .await?;
            Response::json(format!(r#"{{"accepted":{n}}}"#)).status(202)
        }
        ("GET", "/days" | "/days.csv") => {
            let today = sqlite::query("SELECT date(? / 1000, 'unixepoch') AS d", &[&now]).await?.first().expect("a row").get::<String>("d")?;
            let (from, to) = (req.query("from").unwrap_or(&today), req.query("to").unwrap_or(&today));
            let days = sqlite::query(
                "SELECT d.day, d.name, d.events, \
                        (SELECT count(DISTINCT user) FROM events e WHERE e.day = d.day AND e.name = d.name) AS users \
                 FROM daily d WHERE d.day BETWEEN ? AND ? ORDER BY d.day, d.name",
                &[&from, &to],
            )
            .await?;
            if req.path() == "/days" {
                return Ok(Response::json(days.json().to_string()));
            }
            // Names are letters, digits and `_.-`, days digits and `-`: nothing to quote.
            let mut csv = String::from("day,name,events,users\n");
            for row in days.iter() {
                let (day, name): (String, String) = (row.get("day")?, row.get("name")?);
                csv.push_str(&format!("{day},{name},{},{}\n", row.get::<i64>("events")?, row.get::<i64>("users")?));
            }
            Response::bytes("text/csv; charset=utf-8", csv).header("content-disposition", "attachment; filename=\"days.csv\"")
        }
        _ => Response::not_found(),
    })
}

fn bad() -> Response {
    Response::text("an event is {\"name\": 1-64 of [A-Za-z0-9_.-], \"path\", \"user\", \"at\"?}, up to 100 at once\n").status(400)
}

thread_local! {
    static READY: Cell<bool> = const { Cell::new(false) };
}

async fn schema() -> Result<()> {
    if READY.get() {
        return Ok(());
    }
    for sql in [
        "CREATE TABLE IF NOT EXISTS events (at INTEGER NOT NULL, day TEXT NOT NULL, name TEXT NOT NULL, path TEXT, user TEXT)",
        "CREATE INDEX IF NOT EXISTS by_day ON events (day, name, user)",
        "CREATE TABLE IF NOT EXISTS daily (day TEXT, name TEXT, events INTEGER NOT NULL, PRIMARY KEY (day, name))",
        "CREATE TRIGGER IF NOT EXISTS count AFTER INSERT ON events BEGIN \
           INSERT INTO daily VALUES (NEW.day, NEW.name, 1) ON CONFLICT DO UPDATE SET events = events + 1; \
         END",
    ] {
        sqlite::execute(sql, &[]).await?;
    }
    READY.set(true);
    Ok(())
}
