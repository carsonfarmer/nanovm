//! A job queue with retries, on SQLite. Each queue is an object
//! (`/{queue}/`). Producers add jobs, with an idempotency key if they may
//! retry; workers lease a job for a while, then ack it with its result or
//! nack it to retry later. A lease that runs out makes its job visible
//! again, which the next lease finds: nothing runs between requests, so a
//! visibility timeout is a time compared, not a timer. A job that fails
//! `max_attempts` times goes to the dead letters.
//!
//! - `POST /{queue}/jobs` (body: the payload, JSON; `idempotency-key` header)
//! - `POST /{queue}/lease?visibility_ms=30000`: a job and its `lease`, or `204`
//! - `POST /{queue}/jobs/{id}/ack?lease=…` (body: the result), `…/nack?lease=…&delay_ms=…`
//! - `GET /{queue}/jobs/{id}`, `GET /{queue}/` (counts), `GET /{queue}/dead`,
//!   `POST /{queue}/dead/{id}/retry`
//! - `POST /{worker}/work?queue=…&max=10`: any other object, as a worker,
//!   leases jobs from the queue through calls between objects and does them.

use std::cell::Cell;
use std::time::{SystemTime, UNIX_EPOCH};

use nanovm_sdk::{Request, Response, Result, config, object, serde_json, sqlite};
use serde_json::{Value, json};

nanovm_sdk::serve!(handle);

async fn handle(req: Request) -> Result {
    if req.path() == "/work" {
        return work(&req).await;
    }
    schema().await?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64;
    let max: i64 = config::get("max_attempts").ok().flatten().and_then(|m| m.parse().ok()).unwrap_or(3);
    let path: Vec<&str> = req.path().trim_matches('/').split('/').collect();
    let lease = req.query("lease").unwrap_or_default();
    Ok(match (req.method(), path.as_slice()) {
        ("POST", ["jobs"]) => {
            let Ok(payload) = req.json::<Value>() else { return Ok(Response::text("the payload is JSON\n").status(400)) };
            let key = req.header("idempotency-key");
            // The same key again is the same job, whatever became of it.
            let added = sqlite::execute(
                "INSERT INTO jobs (key, payload, state, attempts, visible_at) VALUES (?, ?, 'ready', 0, ?) \
                 ON CONFLICT (key) DO NOTHING",
                &[&key, &payload.to_string(), &now],
            )
            .await?;
            let id = match key {
                Some(key) if added == 0 => sqlite::query("SELECT id FROM jobs WHERE key = ?", &[&key]).await?.first().expect("a job").get("id")?,
                _ => sqlite::last_insert_rowid().await?,
            };
            let job = job(id).await?;
            Response::json(job.to_string()).status(if added == 1 { 201 } else { 200 })
        }
        ("POST", ["lease"]) => {
            let visibility: i64 = req.query_or("visibility_ms", 30_000).clamp(1, 3_600_000);
            // Leases that ran out on their last attempt are dead. (A statement
            // that changes nothing costs no syncs.)
            sqlite::execute(
                "UPDATE jobs SET state = 'dead', error = 'its last lease ran out' \
                 WHERE state = 'leased' AND visible_at <= ? AND attempts >= ?",
                &[&now, &max],
            )
            .await?;
            let token = format!("{:016x}", nanovm_sdk::wasip3::random::random::get_random_u64());
            let leased = sqlite::query(
                "UPDATE jobs SET state = 'leased', attempts = attempts + 1, lease = ?1, visible_at = ?2 + ?3 \
                 WHERE id = (SELECT id FROM jobs WHERE state IN ('ready', 'leased') AND visible_at <= ?2 \
                             ORDER BY visible_at, id LIMIT 1) \
                 RETURNING id",
                &[&token, &now, &visibility],
            )
            .await?;
            match leased.first() {
                Some(row) => Response::json(job(row.get("id")?).await?.to_string()),
                None => Response::new(204),
            }
        }
        ("POST", ["jobs", id, verb @ ("ack" | "nack")]) => {
            let text = req.text().unwrap_or_default();
            let changed = match *verb {
                "ack" => {
                    let sql = "UPDATE jobs SET state = 'done', result = ?3, lease = NULL \
                               WHERE id = ?1 AND lease = ?2 AND state = 'leased'";
                    sqlite::execute(sql, &[id, &lease, &text]).await?
                }
                _ => {
                    let sql = "UPDATE jobs SET state = CASE WHEN attempts >= ?3 THEN 'dead' ELSE 'ready' END, \
                               error = ?4, lease = NULL, visible_at = ?5 WHERE id = ?1 AND lease = ?2 AND state = 'leased'";
                    let visible_at = now + req.query_or("delay_ms", 1000i64).max(0);
                    sqlite::execute(sql, &[id, &lease, &max, &text, &visible_at]).await?
                }
            };
            // A lease that ran out may have gone to another worker.
            if changed == 0 { Response::text("not your lease\n").status(409) } else { Response::new(204) }
        }
        ("GET", ["jobs", id]) => match id.parse() {
            Ok(id) => match job(id).await? {
                Value::Null => Response::not_found(),
                job => Response::json(job.to_string()),
            },
            Err(_) => Response::not_found(),
        },
        ("GET", [""]) => {
            let counts = sqlite::query("SELECT state, count(*) AS n FROM jobs GROUP BY state", &[]).await?;
            let mut out = json!({ "ready": 0, "leased": 0, "done": 0, "dead": 0 });
            for row in counts.iter() {
                out[row.get::<String>("state")?] = row.get::<i64>("n")?.into();
            }
            Response::json(out.to_string())
        }
        ("GET", ["dead"]) => {
            let dead = sqlite::query("SELECT id FROM jobs WHERE state = 'dead' ORDER BY id LIMIT 100", &[]).await?;
            let mut jobs = vec![];
            for row in dead.iter() {
                jobs.push(job(row.get("id")?).await?);
            }
            Response::json(Value::from(jobs).to_string())
        }
        ("POST", ["dead", id, "retry"]) => {
            let changed = sqlite::execute(
                "UPDATE jobs SET state = 'ready', attempts = 0, error = NULL, visible_at = ? WHERE id = ? AND state = 'dead'",
                &[&now, id],
            )
            .await?;
            if changed == 0 { Response::not_found() } else { Response::new(204) }
        }
        _ => Response::not_found(),
    })
}

/// A job as JSON, or `null`.
async fn job(id: i64) -> Result<Value> {
    let rows = sqlite::query(
        "SELECT id, payload, state, attempts, lease, visible_at, result, error FROM jobs WHERE id = ?",
        &[&id],
    )
    .await?;
    let mut job = rows.json().as_array().and_then(|j| j.first().cloned()).unwrap_or(Value::Null);
    if let Some(payload) = job.get("payload").and_then(Value::as_str) {
        job["payload"] = serde_json::from_str(payload)?;
    }
    Ok(job)
}

/// As a worker: leases up to `max` jobs from `queue`, one at a time, and
/// does each: a job `{"text": …}` is done as the text in capitals, and one
/// with `"fail": true` fails. Each ack and nack commits in the queue when
/// the call returns, so a result travels with its ack: a worker that keeps
/// nothing can't lose a result after acking.
async fn work(req: &Request) -> Result {
    let Some(queue) = req.query("queue") else { return Ok(Response::text("queue=…\n").status(400)) };
    let queue = object(queue);
    let (mut done, mut failed) = (0, 0);
    for _ in 0..req.query_or("max", 10).min(100) {
        let reply = queue.post("/lease?visibility_ms=30000", "").send().await?;
        if reply.status != 200 {
            break;
        }
        let job: Value = serde_json::from_slice(&reply.body)?;
        let (id, lease) = (&job["id"], job["lease"].as_str().unwrap_or_default());
        let (path, body) = match job["payload"]["text"].as_str() {
            Some(text) if job["payload"]["fail"] != true => {
                done += 1;
                (format!("/jobs/{id}/ack?lease={lease}"), text.to_uppercase())
            }
            _ => {
                failed += 1;
                let retry = req.query_or("retry_ms", 1000);
                (format!("/jobs/{id}/nack?lease={lease}&delay_ms={retry}"), "could not do it".to_string())
            }
        };
        // An ack the queue failed to commit is safe to send again; one that
        // never lands only means the job is done twice, after its lease.
        for _ in 0..3 {
            if queue.post(&path, body.as_str()).send().await?.status < 500 {
                break;
            }
        }
    }
    Ok(Response::json(json!({ "done": done, "failed": failed }).to_string()))
}

thread_local! {
    static READY: Cell<bool> = const { Cell::new(false) };
}

async fn schema() -> Result<()> {
    if !READY.get() {
        sqlite::execute(
            "CREATE TABLE IF NOT EXISTS jobs (id INTEGER PRIMARY KEY, key TEXT UNIQUE, payload TEXT NOT NULL, \
             state TEXT NOT NULL, attempts INTEGER NOT NULL, visible_at INTEGER NOT NULL, lease TEXT, result TEXT, error TEXT)",
            &[],
        )
        .await?;
        sqlite::execute("CREATE INDEX IF NOT EXISTS next ON jobs (state, visible_at, id)", &[]).await?;
        READY.set(true);
    }
    Ok(())
}
