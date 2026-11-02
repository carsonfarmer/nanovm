//! Composition proof, not a production SQLite provider.
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::limits::Limit;
use rusqlite::types::{Value as SqlValue, ValueRef};
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
thread_local! {
    static OPEN: Cell<bool> = const { Cell::new(false) };
    static RESULT_BYTES: Cell<usize> = const { Cell::new(0) };
    #[cfg(feature = "test-hooks")]
    static YIELDING: Cell<bool> = const { Cell::new(false) };
}
const MAX_RESULT_BYTES: usize = 1024 * 1024;
const MAX_ROWS: usize = 4096;

// Charged across outstanding streams, including streams from a dropped connection.
struct ResultBudget(usize);
impl ResultBudget {
    fn reserve(&mut self, bytes: usize, readonly: bool) -> Result<(), Error> {
        if bytes > MAX_RESULT_BYTES - RESULT_BYTES.get() {
            return Err(result_limit(readonly));
        }
        RESULT_BYTES.set(RESULT_BYTES.get() + bytes);
        self.0 += bytes;
        Ok(())
    }
}
impl Drop for ResultBudget {
    fn drop(&mut self) {
        RESULT_BYTES.set(RESULT_BYTES.get() - self.0);
    }
}
fn result_limit(readonly: bool) -> Error {
    // A RETURNING statement may already have written: never return a catchable
    // limit error that lets the caller commit those otherwise invisible writes.
    assert!(readonly, "SQLite write result limit: request rollback required");
    Error::Io("SQLite result limit exceeded".into())
}

fn error(e: rusqlite::Error) -> Error {
    use rusqlite::ErrorCode::*;
    match e.sqlite_error_code() {
        Some(
            SystemIoFailure
            | DiskFull
            | OperationInterrupted
            | DatabaseCorrupt
            | NotADatabase
            | OutOfMemory
            | InternalMalfunction
            | FileLockingProtocolFailed,
        ) => {
            panic!("SQLite storage failure: request rollback required: {e}");
        }
        Some(AuthorizationForStatementDenied) => Error::AccessDenied,
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
        for (limit, value) in [
            (Limit::SQLITE_LIMIT_LENGTH, 4 * 1024 * 1024),
            (Limit::SQLITE_LIMIT_SQL_LENGTH, 64 * 1024),
            (Limit::SQLITE_LIMIT_COLUMN, 128),
            (Limit::SQLITE_LIMIT_VARIABLE_NUMBER, 128),
        ] {
            db.set_limit(limit, value).map_err(error)?;
        }
        #[cfg(feature = "test-hooks")]
        {
            use rusqlite::functions::FunctionFlags;
            db.create_scalar_function("nanovm_test_arm", 1, FunctionFlags::SQLITE_UTF8, |c| {
                vfs::arm(c.get::<i64>(0)? as u64);
                Ok(0)
            })
            .map_err(error)?;
            db.create_scalar_function("nanovm_test_steps", 0, FunctionFlags::SQLITE_UTF8, |_| {
                Ok(vfs::counts().0 as i64)
            })
            .map_err(error)?;
            db.create_scalar_function("nanovm_test_yield", 1, FunctionFlags::SQLITE_UTF8, |c| {
                YIELDING.set(true);
                util::wasip3::wit_bindgen::block_on(util::write_file("/data", "sql-wait", b"entered".to_vec()))
                    .unwrap();
                util::wasip3::wit_bindgen::block_on(util::wasip3::clocks::monotonic_clock::wait_for(
                    c.get::<i64>(0)? as u64 * 1_000_000,
                ));
                YIELDING.set(false);
                Ok(42)
            })
            .map_err(error)?;
            db.create_scalar_function("nanovm_test_yielding", 0, FunctionFlags::SQLITE_UTF8, |_| {
                Ok(i64::from(YIELDING.get()))
            })
            .map_err(error)?;
        }
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
        // No async yield while SQLite is executing. The VFS's block_on waits
        // for host I/O without letting another call re-enter this component.
        if statement.len() > 64 * 1024
            || parameters.len() > 128
            || parameters
                .iter()
                .map(|v| match v {
                    Value::Text(v) => v.len(),
                    Value::Blob(v) => v.len(),
                    _ => size_of::<Value>(),
                })
                .sum::<usize>()
                > MAX_RESULT_BYTES
        {
            return Err(Error::Io("SQLite input limit exceeded".into()));
        }
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
        let readonly = stmt.readonly();
        let mut budget = ResultBudget(0);
        for name in stmt.column_names() {
            budget.reserve(size_of::<String>() + name.len(), readonly)?;
        }
        let columns = stmt.column_names().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut cursor = stmt.query(rusqlite::params_from_iter(parameters)).map_err(error)?;
        let mut rows = Vec::new();
        while let Some(row) = cursor.next().map_err(error)? {
            if rows.len() == MAX_ROWS {
                return Err(result_limit(readonly));
            }
            budget.reserve(size_of::<RowResult>() + columns.len() * size_of::<Value>(), readonly)?;
            let mut values = Vec::with_capacity(columns.len());
            for i in 0..columns.len() {
                let payload = match row.get_ref(i).map_err(error)? {
                    ValueRef::Text(v) | ValueRef::Blob(v) => v.len(),
                    _ => 0,
                };
                budget.reserve(payload, readonly)?;
                values.push(
                    match row.get::<_, SqlValue>(i).map_err(|e| {
                        // SQLite text need not be UTF-8, but WIT strings must be.
                        // Like result overflow, conversion can fail after a write.
                        assert!(
                            readonly,
                            "SQLite write result conversion: request rollback required: {e}"
                        );
                        error(e)
                    })? {
                        SqlValue::Integer(v) => Value::Integer(v),
                        SqlValue::Real(v) => Value::Real(v),
                        SqlValue::Text(v) => Value::Text(v),
                        SqlValue::Blob(v) => Value::Blob(v),
                        SqlValue::Null => Value::Null,
                    },
                );
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
            drop(budget);
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
        self.last_insert_rowid()
    }
    async fn changes_async(&self) -> u64 {
        self.changes()
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        OPEN.set(false);
    }
}
