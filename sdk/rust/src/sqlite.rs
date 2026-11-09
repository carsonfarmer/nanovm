//! The object's SQLite database: Spin's `spin:sqlite` interface, served by a
//! Wasm provider composed into the app (`tools/build-app.sh` does it when
//! the app uses this module) over the object's own volume. The database is
//! files in `/data`, so it commits with the request like every other change.
//!
//! ```no_run
//! # async fn f() -> nanovm_sdk::Result<()> {
//! use nanovm_sdk::sqlite;
//! sqlite::execute("CREATE TABLE IF NOT EXISTS scores(player TEXT PRIMARY KEY, score INTEGER)", &[]).await?;
//! sqlite::execute("INSERT INTO scores VALUES (?, ?) ON CONFLICT DO UPDATE SET score = max(score, excluded.score)", &[&"ann", &12]).await?;
//! for row in sqlite::query("SELECT player, score FROM scores ORDER BY score DESC", &[]).await?.iter() {
//!     let (player, score): (String, i64) = (row.get("player")?, row.get("score")?);
//! }
//! # Ok(()) }
//! ```
//!
//! **The request is the transaction.** `BEGIN`, `COMMIT`, savepoints,
//! `ATTACH` and `PRAGMA`s are refused: every statement a request runs
//! commits with it, together with its file and key-value changes, or rolls
//! back with them when the handler returns an error. So return the error
//! (`?`) when a later step fails; answering `409` instead would keep what
//! the earlier statements did.
//!
//! A statement returns at most 4,096 rows and 1 MiB; page with `LIMIT`.

mod bindings {
    wit_bindgen::generate!({ path: "../../components/sqlite/wit", world: "client", generate_all });
}

pub use bindings::spin::sqlite::sqlite::{Connection, Error, Value};

thread_local! {
    // Opened at first use and kept for the instance's life: the provider
    // allows one connection per object, and a request that fails discards
    // the whole instance, connection included.
    static DB: std::cell::OnceCell<&'static Connection> = const { std::cell::OnceCell::new() };
}

/// The object's database (`default`), opened at first use.
pub async fn db() -> Result<&'static Connection, Error> {
    if let Some(db) = DB.with(|d| d.get().copied()) {
        return Ok(db);
    }
    let db: &'static Connection = Box::leak(Box::new(Connection::open_async("default".into()).await?));
    Ok(DB.with(|d| *d.get_or_init(|| db)))
}

/// Runs `sql`, with `?` placeholders bound to `params` in order, and returns
/// the rows it produced.
pub async fn query(sql: &str, params: &[&dyn Param]) -> Result<Rows, Error> {
    let params = params.iter().map(|p| p.value()).collect();
    let (columns, rows, done) = db().await?.execute_async(sql.into(), params).await?;
    let rows = rows.collect().await;
    done.await?;
    Ok(Rows { columns, rows: rows.into_iter().map(|r| r.values).collect() })
}

/// Runs `sql` for its effect: the number of rows it changed.
pub async fn execute(sql: &str, params: &[&dyn Param]) -> Result<u64, Error> {
    query(sql, params).await?;
    Ok(db().await?.changes_async().await)
}

/// The rowid of the last row inserted.
pub async fn last_insert_rowid() -> Result<i64, Error> {
    Ok(db().await?.last_insert_rowid_async().await)
}

/// What a statement returned.
#[derive(Debug, Clone)]
pub struct Rows {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

impl Rows {
    pub fn iter(&self) -> impl Iterator<Item = Row<'_>> {
        self.rows.iter().map(|values| Row { columns: &self.columns, values })
    }

    pub fn first(&self) -> Option<Row<'_>> {
        self.iter().next()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The rows as a JSON array of objects, column name to value (a blob as
    /// an array of bytes).
    #[cfg(feature = "json")]
    pub fn json(&self) -> serde_json::Value {
        let value = |v: &Value| match v {
            Value::Integer(i) => (*i).into(),
            Value::Real(f) => (*f).into(),
            Value::Text(s) => s.as_str().into(),
            Value::Blob(b) => b.as_slice().into(),
            Value::Null => serde_json::Value::Null,
        };
        let row = |r: Row| r.columns.iter().cloned().zip(r.values.iter().map(value)).collect::<serde_json::Map<_, _>>();
        self.iter().map(row).collect()
    }
}

/// One row of [`Rows`].
#[derive(Clone, Copy)]
pub struct Row<'a> {
    columns: &'a [String],
    values: &'a [Value],
}

impl Row<'_> {
    /// Column `name` as a `T`: `i64`, `f64`, `String`, `Vec<u8>`, `bool`, or an
    /// `Option` of one for a column that may be `NULL`.
    pub fn get<T: FromValue>(&self, name: &str) -> Result<T, Error> {
        let i = self.columns.iter().position(|c| c == name);
        let i = i.ok_or_else(|| Error::Io(format!("no column {name:?}")))?;
        T::from_value(&self.values[i]).ok_or_else(|| Error::Io(format!("column {name:?} is {:?}", self.values[i])))
    }

    /// The values, in column order.
    pub fn values(&self) -> &[Value] {
        self.values
    }
}

/// A value a statement's `?` can be bound to.
pub trait Param {
    fn value(&self) -> Value;
}

impl Param for Value {
    fn value(&self) -> Value {
        self.clone()
    }
}

macro_rules! integer {
    ($($t:ty)*) => {$(
        impl Param for $t {
            fn value(&self) -> Value { Value::Integer(*self as i64) }
        }
    )*};
}
integer!(i64 i32 i16 i8 u32 u16 u8 usize bool);

impl Param for u64 {
    /// Larger than `i64::MAX` wraps, as SQLite has no unsigned integers.
    fn value(&self) -> Value {
        Value::Integer(*self as i64)
    }
}

impl Param for f64 {
    fn value(&self) -> Value {
        Value::Real(*self)
    }
}

impl Param for &str {
    fn value(&self) -> Value {
        Value::Text(self.to_string())
    }
}

impl Param for String {
    fn value(&self) -> Value {
        Value::Text(self.clone())
    }
}

impl Param for &[u8] {
    fn value(&self) -> Value {
        Value::Blob(self.to_vec())
    }
}

impl Param for Vec<u8> {
    fn value(&self) -> Value {
        Value::Blob(self.clone())
    }
}

impl<T: Param> Param for Option<T> {
    fn value(&self) -> Value {
        self.as_ref().map_or(Value::Null, T::value)
    }
}

/// A column value [`Row::get`] can read.
pub trait FromValue: Sized {
    fn from_value(value: &Value) -> Option<Self>;
}

impl FromValue for i64 {
    fn from_value(value: &Value) -> Option<i64> {
        match value {
            Value::Integer(i) => Some(*i),
            _ => None,
        }
    }
}

impl FromValue for bool {
    fn from_value(value: &Value) -> Option<bool> {
        i64::from_value(value).map(|i| i != 0)
    }
}

impl FromValue for f64 {
    fn from_value(value: &Value) -> Option<f64> {
        match value {
            Value::Real(f) => Some(*f),
            Value::Integer(i) => Some(*i as f64),
            _ => None,
        }
    }
}

impl FromValue for String {
    fn from_value(value: &Value) -> Option<String> {
        match value {
            Value::Text(s) => Some(s.clone()),
            _ => None,
        }
    }
}

impl FromValue for Vec<u8> {
    fn from_value(value: &Value) -> Option<Vec<u8>> {
        match value {
            Value::Blob(b) => Some(b.clone()),
            Value::Text(s) => Some(s.clone().into_bytes()),
            _ => None,
        }
    }
}

impl<T: FromValue> FromValue for Option<T> {
    fn from_value(value: &Value) -> Option<Option<T>> {
        match value {
            Value::Null => Some(None),
            v => T::from_value(v).map(Some),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_read_by_column() {
        let rows = Rows {
            columns: vec!["name".into(), "n".into(), "score".into(), "note".into()],
            rows: vec![vec![Value::Text("ann".into()), Value::Integer(3), Value::Real(2.5), Value::Null]],
        };
        let row = rows.first().unwrap();
        assert_eq!(row.get::<String>("name").unwrap(), "ann");
        assert_eq!((row.get::<i64>("n").unwrap(), row.get::<f64>("n").unwrap()), (3, 3.0));
        assert_eq!(row.get::<Option<String>>("note").unwrap(), None);
        assert_eq!(row.get::<Option<i64>>("n").unwrap(), Some(3));
        assert!(row.get::<i64>("name").is_err() && row.get::<String>("note").is_err() && row.get::<i64>("x").is_err());
        assert!(matches!(Some("x").value(), Value::Text(t) if t == "x"));
        assert!(matches!(None::<i64>.value(), Value::Null));
        assert!(matches!(true.value(), Value::Integer(1)));
        #[cfg(feature = "json")]
        assert_eq!(rows.json(), serde_json::json!([{"name":"ann","n":3,"score":2.5,"note":null}]));
    }
}
