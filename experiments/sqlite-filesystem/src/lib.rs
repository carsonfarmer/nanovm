//! Experiment only: real SQLite inside Wasm; no native SQLite host calls.
use rusqlite::Connection;
use util::wasip3;
use wasip3::http::types::{ErrorCode, Request, Response};

#[cfg(not(feature = "stock"))]
mod bench;
#[cfg(not(feature = "stock"))]
mod pressure;
#[cfg(not(feature = "stock"))]
mod vfs;

wasip3::http::service::export!(Sqlite);
struct Sqlite;

impl wasip3::exports::http::handler::Guest for Sqlite {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let req = util::read_req(request).await;
        #[cfg(not(feature = "stock"))]
        if req.path == "/bench" {
            return bench::run(req).await;
        }
        #[cfg(not(feature = "stock"))]
        if req.path == "/pressure" {
            return pressure::run(req).await;
        }
        #[cfg(not(feature = "stock"))]
        let db = Connection::open_with_flags_and_vfs(
            "probe.db",
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
            "wasi-p3",
        );
        #[cfg(feature = "stock")]
        let db = Connection::open("/data/probe.db");
        let db = db.map_err(sql_error)?;
        db.execute_batch(
            "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA temp_store=MEMORY;
            CREATE TABLE IF NOT EXISTS ledger(id INTEGER PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS blobs(value BLOB);",
        )
        .map_err(sql_error)?;
        if let Some(value) = req.q("v") {
            db.execute("INSERT INTO ledger(value) VALUES (?)", [value])
                .map_err(sql_error)?;
            util::write_file("/data", "marker", value.as_bytes().to_vec())
                .await
                .unwrap();
            util::kv::store::open("")
                .unwrap()
                .set("marker", value.as_bytes())
                .unwrap();
        }
        if req.path == "/full" {
            // More than the test object's entire quota. Propagate the error so
            // nanovm discards this request, including earlier mixed writes.
            db.execute("INSERT INTO blobs VALUES (zeroblob(2097152))", [])
                .map_err(sql_error)?;
        }
        if req.path == "/constraint" {
            db.execute("INSERT INTO ledger(id,value) VALUES (1,'duplicate')", [])
                .unwrap_err();
        }
        if req.path == "/grow" {
            db.execute("INSERT INTO blobs VALUES (zeroblob(262144))", [])
                .map_err(sql_error)?;
        }
        if req.path == "/transaction" {
            db.execute_batch("BEGIN; INSERT INTO ledger(value) VALUES ('inner-rollback'); ROLLBACK;")
                .map_err(sql_error)?;
        }
        if req.path == "/vacuum" {
            db.execute_batch("DELETE FROM blobs; VACUUM;").map_err(sql_error)?;
        }
        // Close SQLite before returning, just as a guest library would. For
        // trap/timeout tests leave it open to test instance teardown too.
        match req.q("then") {
            Some("panic") => panic!("after SQLite, file and KV writes"),
            Some("error") => return Err(ErrorCode::InternalError(Some("after writes".into()))),
            Some("sql") => {
                db.query_row::<i64, _, _>(
                    "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000000000) SELECT sum(x) FROM n",
                    [], |r| r.get(0),
                ).map_err(sql_error)?;
            }
            Some("spin") => loop {
                std::hint::black_box(0);
            },
            Some("sleep") => wasip3::clocks::monotonic_clock::wait_for(60_000_000_000).await,
            _ => (),
        }
        let rows = db
            .prepare("SELECT value FROM ledger ORDER BY id")
            .map_err(sql_error)?
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        let integrity: String = db
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .map_err(sql_error)?;
        let blob_bytes: i64 = db
            .query_row("SELECT coalesce(sum(length(value)),0) FROM blobs", [], |r| r.get(0))
            .map_err(sql_error)?;
        let file = util::read_file("/data", "marker").await.unwrap_or_default();
        let key = util::kv::store::open("")
            .unwrap()
            .get("marker")
            .unwrap()
            .unwrap_or_default();
        db.close().map_err(|(_, e)| sql_error(e))?;
        util::json(
            if req.q("then") == Some("status") { 507 } else { 200 },
            serde_json::json!({"rows": rows, "integrity": integrity, "blob_bytes": blob_bytes,
                "file": String::from_utf8(file).unwrap(), "key": String::from_utf8(key).unwrap()})
            .to_string(),
        )
    }
}

fn sql_error(e: rusqlite::Error) -> ErrorCode {
    ErrorCode::InternalError(Some(format!("sqlite: {e}")))
}
