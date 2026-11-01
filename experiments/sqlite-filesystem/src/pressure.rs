//! Adversarial workloads, kept separate from the small original smoke test.
use crate::{sql_error, vfs};
use rusqlite::Connection;
use std::cell::RefCell;
use util::wasip3::http::types::{ErrorCode, Response};

thread_local! {
    static CONNECTION: RefCell<Option<Connection>> = const { RefCell::new(None) };
}

pub async fn run(req: util::Req) -> Result<Response, ErrorCode> {
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
            db.execute_batch("ATTACH 'aux.db' AS aux;").map_err(sql_error)?;
            db
        }
    };
    let mode = match req.q("mode").unwrap_or("DELETE") {
        "DELETE" => "DELETE",
        "TRUNCATE" => "TRUNCATE",
        "PERSIST" => "PERSIST",
        "MEMORY" => "MEMORY",
        "WAL" => "WAL",
        _ => return util::text(400, "unsupported test mode"),
    };
    let main_mode: String = db
        .query_row(&format!("PRAGMA main.journal_mode={mode}"), [], |r| r.get(0))
        .map_err(sql_error)?;
    let aux_mode: String = db
        .query_row(&format!("PRAGMA aux.journal_mode={mode}"), [], |r| r.get(0))
        .map_err(sql_error)?;
    db.execute_batch(
        "PRAGMA main.synchronous=FULL; PRAGMA aux.synchronous=FULL;
        PRAGMA cache_size=4; PRAGMA cache_spill=ON; PRAGMA temp_store=MEMORY;
        CREATE TABLE IF NOT EXISTS items(id INTEGER PRIMARY KEY, value INTEGER, payload BLOB);
        CREATE TABLE IF NOT EXISTS aux.tally(value INTEGER);",
    )
    .map_err(sql_error)?;

    let op = req.q("op").unwrap_or("read");
    if op == "init" {
        db.execute_batch(
            "BEGIN; DELETE FROM items; DELETE FROM aux.tally; INSERT INTO aux.tally VALUES (29);
            WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<16)
            INSERT INTO items SELECT x,x*3,zeroblob(1024) FROM n; COMMIT;",
        )
        .map_err(sql_error)?;
        marker("before").await;
    }
    vfs::arm(req.q_u64("cut", 0));
    let mut completed = 0u64;
    let mut handled = None;
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
            for _ in 0..req.q_u64("n", 100) {
                match db.execute("UPDATE items SET value=value+1 WHERE id=1", []) {
                    Ok(_) => completed += 1,
                    Err(e) if req.q("handle") == Some("1") => {
                        handled = Some(e.to_string());
                        break;
                    }
                    Err(e) => return Err(sql_error(e)),
                }
            }
            if handled.is_none() && req.q("bulk") == Some("1") {
                if let Err(e) = db.execute("UPDATE items SET value=value+7,payload=zeroblob(2048)", []) {
                    if req.q("handle") == Some("1") {
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
        return util::json(
            200,
            serde_json::json!({"completed": completed, "handled": handled}).to_string(),
        );
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
    let file = util::read_file("/data", "pressure-marker").await.unwrap();
    let key = util::kv::store::open("")
        .unwrap()
        .get("pressure-marker")
        .unwrap()
        .unwrap();
    if req.q("cache") == Some("1") {
        CONNECTION.with(|c| *c.borrow_mut() = Some(db));
    } else {
        db.close().map_err(|(_, e)| sql_error(e))?;
    }
    let (steps, bytes) = vfs::counts();
    util::json(
        200,
        serde_json::json!({
            "state": {"rows": rows, "tally": tally, "integrity": integrity, "aux_integrity": aux_integrity,
                "file": String::from_utf8(file).unwrap(), "key": String::from_utf8(key).unwrap()},
            "mode": main_mode, "aux_mode": aux_mode, "reused": reused, "steps": steps, "write_bytes": bytes,
            "completed": completed, "handled": handled,
        })
        .to_string(),
    )
}

async fn marker(value: &str) {
    util::write_file("/data", "pressure-marker", value.as_bytes().to_vec())
        .await
        .unwrap();
    util::kv::store::open("")
        .unwrap()
        .set("pressure-marker", value.as_bytes())
        .unwrap();
}
