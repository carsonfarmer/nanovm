//! Adversarial workloads, kept separate from the small original smoke test.
use crate::{sql_error, vfs};
use rusqlite::Connection;
use std::cell::RefCell;
use nanovm_sdk::wasip3::http::types::{ErrorCode, Response};

thread_local! {
    static CONNECTION: RefCell<Option<Connection>> = const { RefCell::new(None) };
}

pub async fn run(req: nanovm_sdk::Request) -> Result<Response, ErrorCode> {
    // A previous successful request may have kept the SQLite page cache alive.
    vfs::arm(0);
    let cached = CONNECTION.with(|c| c.borrow_mut().take());
    let reused = cached.is_some();
    let db = match cached {
        Some(db) => db,
        None => {
            let db = Connection::open_with_flags_and_vfs(
                "pressure.db",
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
                "wasi-p3",
            )
            .map_err(sql_error)?;
            if req.query("exclusive") == Some("1") {
                // Before ATTACH or any access to a persisted WAL database.
                db.execute_batch("PRAGMA locking_mode=EXCLUSIVE;").map_err(sql_error)?;
            }
            db.execute_batch("ATTACH 'aux.db' AS aux;").map_err(sql_error)?;
            db
        }
    };
    let mode = match req.query("mode").unwrap_or("DELETE") {
        "DELETE" => "DELETE",
        "TRUNCATE" => "TRUNCATE",
        "PERSIST" => "PERSIST",
        "MEMORY" => "MEMORY",
        "WAL" => "WAL",
        _ => return nanovm_sdk::Response::text("unsupported test mode").status(400).into_wit(),
    };
    let main_mode: String = db
        .query_row(&format!("PRAGMA main.journal_mode={mode}"), [], |r| r.get(0))
        .map_err(sql_error)?;
    let aux_mode: String = db
        .query_row(&format!("PRAGMA aux.journal_mode={mode}"), [], |r| r.get(0))
        .map_err(sql_error)?;
    if let Some(pages) = req.query("autocheckpoint") {
        let pages: u32 = pages.parse().unwrap();
        db.execute_batch(&format!("PRAGMA wal_autocheckpoint={pages};"))
            .map_err(sql_error)?;
    }
    db.execute_batch(
        "PRAGMA main.synchronous=FULL; PRAGMA aux.synchronous=FULL;
        PRAGMA cache_size=4; PRAGMA cache_spill=ON; PRAGMA temp_store=MEMORY;
        CREATE TABLE IF NOT EXISTS items(id INTEGER PRIMARY KEY, value INTEGER, payload BLOB);
        CREATE TABLE IF NOT EXISTS aux.tally(value INTEGER);",
    )
    .map_err(sql_error)?;

    let op = req.query("op").unwrap_or("read");
    if op == "init" {
        db.execute_batch(
            "BEGIN; DELETE FROM items; DELETE FROM aux.tally; INSERT INTO aux.tally VALUES (29);
            WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<16)
            INSERT INTO items SELECT x,x*3,zeroblob(1024) FROM n; COMMIT;",
        )
        .map_err(sql_error)?;
        marker("before").await;
    }
    vfs::arm(req.query_or("cut", 0u64));
    let mut completed = 0u64;
    let mut handled = None;
    let mut checkpoints = vec![];
    if op == "checkpoint" {
        let checkpoint = match req.query("checkpoint").unwrap_or("PASSIVE") {
            "PASSIVE" => "PASSIVE",
            "FULL" => "FULL",
            "RESTART" => "RESTART",
            "TRUNCATE" => "TRUNCATE",
            _ => return nanovm_sdk::Response::text("unsupported checkpoint").status(400).into_wit(),
        };
        for name in ["main", "aux"] {
            let result = db
                .query_row(&format!("PRAGMA {name}.wal_checkpoint({checkpoint})"), [], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
                })
                .map_err(sql_error)?;
            checkpoints.push(result);
        }
    }
    if op == "mutate" || op == "churn" {
        marker("after").await;
        if op == "mutate" {
            db.execute_batch(
                "BEGIN IMMEDIATE;
                UPDATE items SET value=value+7,payload=zeroblob(2048);
                SAVEPOINT inner;
                DELETE FROM items WHERE id%2=0;
                INSERT INTO items VALUES (99,999,zeroblob(8192));
                ROLLBACK TO inner; RELEASE inner;
                UPDATE aux.tally SET value=value+11;
                COMMIT;",
            )
            .map_err(sql_error)?;
        } else {
            for _ in 0..req.query_or("n", 100u64) {
                match db.execute("UPDATE items SET value=value+1 WHERE id=1", []) {
                    Ok(_) => completed += 1,
                    Err(e) if req.query("handle") == Some("1") => {
                        handled = Some(e.to_string());
                        break;
                    }
                    Err(e) => return Err(sql_error(e)),
                }
            }
            if handled.is_none() && req.query("full").is_some() {
                // Both statements exceed the unchanged 1 MiB volume quota.
                let sql = if req.query("full") == Some("bulk") {
                    "UPDATE items SET value=value+7,payload=zeroblob(131072)"
                } else {
                    "INSERT INTO items VALUES (99,999,zeroblob(2097152))"
                };
                if let Err(e) = db.execute(sql, []) {
                    if req.query("handle") == Some("1") {
                        handled = Some(e.to_string());
                    } else {
                        return Err(sql_error(e));
                    }
                }
            }
        }
    }
    if handled.is_some() {
        // Deliberately swallow the storage error and return success without
        // another query. The host must not rely on a cooperative guest.
        drop(db);
        return nanovm_sdk::Response::json(serde_json::json!({"completed": completed, "handled": handled}).to_string()).status(200).into_wit();
    }
    let rows = db
        .prepare("SELECT id,value,length(payload) FROM items ORDER BY id")
        .map_err(sql_error)?
        .query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        })
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?;
    let tally: i64 = db
        .query_row("SELECT value FROM aux.tally", [], |r| r.get(0))
        .map_err(sql_error)?;
    let integrity: String = db
        .query_row("PRAGMA main.integrity_check", [], |r| r.get(0))
        .map_err(sql_error)?;
    let aux_integrity: String = db
        .query_row("PRAGMA aux.integrity_check", [], |r| r.get(0))
        .map_err(sql_error)?;
    let file = nanovm_sdk::data().read("pressure-marker").await.unwrap().unwrap();
    let key = nanovm_sdk::kv::store::open("")
        .unwrap()
        .get("pressure-marker")
        .unwrap()
        .unwrap();
    if req.query("cache") == Some("1") {
        CONNECTION.with(|c| *c.borrow_mut() = Some(db));
    } else {
        db.close().map_err(|(_, e)| sql_error(e))?;
    }
    if req.query("then") == Some("error") {
        return Err(ErrorCode::InternalError(Some(format!(
            "after {completed} completed statements"
        ))));
    }
    let (steps, bytes) = vfs::counts();
    nanovm_sdk::Response::json(serde_json::json!({
            "state": {"rows": rows, "tally": tally, "integrity": integrity, "aux_integrity": aux_integrity,
                "file": String::from_utf8(file).unwrap(), "key": String::from_utf8(key).unwrap()},
            "mode": main_mode, "aux_mode": aux_mode, "reused": reused, "steps": steps, "write_bytes": bytes,
            "completed": completed, "handled": handled, "checkpoints": checkpoints,
        })
        .to_string()).status(200).into_wit()
}

async fn marker(value: &str) {
    nanovm_sdk::data().write("pressure-marker", value.as_bytes().to_vec())
        .await
        .unwrap();
    nanovm_sdk::kv::store::open("")
        .unwrap()
        .set("pressure-marker", value.as_bytes())
        .unwrap();
}
