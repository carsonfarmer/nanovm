//! Benchmark workloads omit the pressure fixture's per-request integrity scans.
use crate::{sql_error, vfs};
use rusqlite::Connection;
use std::cell::RefCell;
use util::wasip3::http::types::{ErrorCode, Response};

thread_local! {
    static CONNECTION: RefCell<Option<Connection>> = const { RefCell::new(None) };
}

pub async fn run(req: util::Req) -> Result<Response, ErrorCode> {
    vfs::arm(0);
    let cached = CONNECTION.with(|c| c.borrow_mut().take());
    let db = match cached {
        Some(db) => db,
        None => {
            let db = Connection::open_with_flags_and_vfs(
                "benchmark.db",
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
                "wasi-p3",
            )
            .map_err(sql_error)?;
            let mode = if req.q("mode") == Some("WAL") {
                db.execute_batch("PRAGMA locking_mode=EXCLUSIVE;").map_err(sql_error)?;
                "WAL"
            } else {
                "DELETE"
            };
            let actual: String = db
                .query_row(&format!("PRAGMA journal_mode={mode}"), [], |r| r.get(0))
                .map_err(sql_error)?;
            assert_eq!(actual, mode.to_lowercase());
            db.execute_batch(&format!(
                "PRAGMA synchronous=FULL; PRAGMA cache_size=-2048; PRAGMA temp_store=MEMORY; PRAGMA wal_autocheckpoint={};",
                req.q_u64("pages", 1000),
            )).map_err(sql_error)?;
            db
        }
    };
    let op = req.q("op").unwrap_or("point");
    let seq = req.q_u64("seq", 0);
    let mut value = 0i64;
    let mut integrity = None;
    match op {
        "init" => db
            .execute_batch(
                "CREATE TABLE items(id INTEGER PRIMARY KEY, value INTEGER NOT NULL, payload BLOB NOT NULL);
            BEGIN; WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<1023)
            INSERT INTO items SELECT x,0,zeroblob(512) FROM n; COMMIT;
            PRAGMA wal_checkpoint(TRUNCATE);",
            )
            .map_err(sql_error)?,
        "point" => {
            value = db
                .query_row(
                    "SELECT length(payload) FROM items WHERE id=?",
                    [(seq * 37 % 1024) as i64],
                    |r| r.get(0),
                )
                .map_err(sql_error)?;
        }
        "scan" => {
            value = db
                .query_row("SELECT sum(length(payload)) FROM items", [], |r| r.get(0))
                .map_err(sql_error)?;
        }
        "write" | "batch" | "autocommit" | "blob" => {
            let n = if op == "batch" || op == "autocommit" { 100 } else { 1 };
            if op == "batch" {
                db.execute_batch("BEGIN IMMEDIATE;").map_err(sql_error)?;
            }
            let sql = if op == "blob" {
                "UPDATE items SET value=value+1,payload=?2 WHERE id=?1"
            } else {
                "UPDATE items SET value=value+1 WHERE id=?"
            };
            let mut statement = db.prepare(sql).map_err(sql_error)?;
            for i in 0..n {
                let id = if op == "autocommit" || op == "blob" {
                    0
                } else {
                    (seq * 37 + i) % 1024
                };
                value += if op == "blob" {
                    // Change every byte, not just the value in the first page.
                    statement.execute(rusqlite::params![id as i64, vec![seq as u8; 65536]])
                } else {
                    statement.execute([id as i64])
                }
                .map_err(sql_error)? as i64;
            }
            drop(statement);
            if op == "batch" {
                db.execute_batch("COMMIT;").map_err(sql_error)?;
            }
        }
        "verify" => {
            value = db
                .query_row("SELECT sum(value) FROM items", [], |r| r.get(0))
                .map_err(sql_error)?;
            integrity = Some(
                db.query_row::<String, _, _>("PRAGMA integrity_check", [], |r| r.get(0))
                    .map_err(sql_error)?,
            );
        }
        "close" => (),
        _ => return util::text(400, "unknown workload"),
    }
    if req.q("cache") == Some("1") && op != "close" && op != "verify" {
        CONNECTION.with(|c| *c.borrow_mut() = Some(db));
    } else {
        db.close().map_err(|(_, e)| sql_error(e))?;
    }
    let (_, bytes) = vfs::counts();
    util::json(
        200,
        serde_json::json!({"value": value, "integrity": integrity,
        "sqlite_write_bytes": bytes, "sqlite_syncs": vfs::syncs()})
        .to_string(),
    )
}
