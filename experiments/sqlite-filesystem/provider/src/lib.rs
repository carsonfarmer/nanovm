//! Composition proof, not a production SQLite provider.
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::types::Value as SqlValue;
use std::cell::Cell;

#[allow(dead_code)]
#[path = "../../src/vfs.rs"]
mod vfs;

mod bindings {
    wit_bindgen::generate!({ path: "../wit", world: "provider" });
}
use bindings::exports::spin::sqlite::sqlite::{
    Connection, Error, Guest, GuestConnection, QueryResult, RowResult, Value,
};
bindings::export!(Provider with_types_in bindings);

struct Provider;
pub struct Database(rusqlite::Connection, Slot);
// Reserve before any filesystem suspension; failed opens release the slot too.
struct Slot;
thread_local! { static OPEN: Cell<bool> = const { Cell::new(false) }; }

fn error(e: rusqlite::Error) -> Error {
    match e.sqlite_error_code() {
        Some(rusqlite::ErrorCode::DiskFull) => Error::DatabaseFull,
        Some(rusqlite::ErrorCode::AuthorizationForStatementDenied) => Error::AccessDenied,
        _ => Error::Io(e.to_string()),
    }
}

impl Guest for Provider {
    type Connection = Database;
}

impl GuestConnection for Database {
    fn open(_database: String) -> Result<Connection, Error> {
        Err(Error::Io("P3 filesystem requires open-async".into()))
    }

    fn execute(&self, _statement: String, _parameters: Vec<Value>) -> Result<QueryResult, Error> {
        Err(Error::Io("P3 filesystem requires execute-async".into()))
    }

    async fn open_async(database: String) -> Result<Connection, Error> {
        if database != "default" {
            return Err(Error::NoSuchDatabase);
        }
        if OPEN.replace(true) {
            return Err(Error::InvalidConnection);
        }
        let slot = Slot;
        let mut db = rusqlite::Connection::open_with_flags_and_vfs(
            "composed.db",
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
            "wasi-p3",
        )
        .map_err(error)?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA temp_store=MEMORY;")
            .map_err(error)?;
        db.authorizer(Some(|ctx: AuthContext<'_>| match ctx.action {
            AuthAction::Transaction { .. }
            | AuthAction::Savepoint { .. }
            | AuthAction::Attach { .. }
            | AuthAction::Detach { .. } => Authorization::Deny,
            AuthAction::Pragma {
                pragma_name,
                pragma_value: None,
            } if pragma_name.eq_ignore_ascii_case("integrity_check") => Authorization::Allow,
            AuthAction::Pragma { .. } => Authorization::Deny,
            _ => Authorization::Allow,
        }))
        .map_err(error)?;
        Ok(Connection::new(Database(db, slot)))
    }

    async fn execute_async(
        &self,
        statement: String,
        parameters: Vec<Value>,
    ) -> Result<
        (
            Vec<String>,
            wit_bindgen::rt::async_support::StreamReader<RowResult>,
            wit_bindgen::rt::async_support::FutureReader<Result<(), Error>>,
        ),
        Error,
    > {
        let parameters: Vec<SqlValue> = parameters
            .into_iter()
            .map(|v| match v {
                Value::Integer(v) => SqlValue::Integer(v),
                Value::Real(v) => SqlValue::Real(v),
                Value::Text(v) => SqlValue::Text(v),
                Value::Blob(v) => SqlValue::Blob(v),
                Value::Null => SqlValue::Null,
            })
            .collect();
        let mut stmt = self.0.prepare(&statement).map_err(error)?;
        let columns = stmt.column_names().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut cursor = stmt.query(rusqlite::params_from_iter(parameters)).map_err(error)?;
        let mut rows = Vec::new();
        while let Some(row) = cursor.next().map_err(error)? {
            let mut values = Vec::new();
            for i in 0..columns.len() {
                values.push(match row.get::<_, SqlValue>(i).map_err(error)? {
                    SqlValue::Integer(v) => Value::Integer(v),
                    SqlValue::Real(v) => Value::Real(v),
                    SqlValue::Text(v) => Value::Text(v),
                    SqlValue::Blob(v) => Value::Blob(v),
                    SqlValue::Null => Value::Null,
                });
            }
            rows.push(RowResult { values });
        }
        // Complete SQLite execution before streaming; no live SQL cursor crosses a response.
        drop(cursor);
        drop(stmt);
        let (mut tx, rx) = bindings::wit_stream::new();
        let (done_tx, done_rx) = bindings::wit_future::new(|| Ok(()));
        wit_bindgen::spawn_local(async move {
            let _ = tx.write_all(rows).await;
            drop(tx);
            drop(done_tx);
        });
        Ok((columns, rx, done_rx))
    }
    fn last_insert_rowid(&self) -> i64 {
        self.0.last_insert_rowid()
    }
    fn changes(&self) -> u64 {
        self.0.changes()
    }
    async fn last_insert_rowid_async(&self) -> i64 {
        self.0.last_insert_rowid()
    }
    async fn changes_async(&self) -> u64 {
        self.0.changes()
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        OPEN.set(false);
    }
}
