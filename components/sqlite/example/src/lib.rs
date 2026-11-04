//! A small app using SQL, files and KV in the same request transaction.
use nanovm_sdk::{Error, Request, Response, Result, data, kv};
mod bindings {
    wit_bindgen::generate!({ path: "../wit", world: "client", generate_all });
}
use bindings::spin::sqlite::sqlite::{Connection, QueryResult, Value};

nanovm_sdk::serve!(handle);

async fn query(db: &Connection, sql: &str, parameters: Vec<Value>) -> QueryResult {
    // A failure must abort this request, rather than return an HTTP error that
    // would commit earlier SQL/file/KV writes. A real app can handle known-safe
    // validation errors before it starts mutating state.
    let (columns, mut stream, done) = db.execute_async(sql.to_string(), parameters).await.unwrap();
    let mut rows = Vec::new();
    loop {
        let (_, batch) = stream.read(Vec::with_capacity(16)).await;
        if batch.is_empty() {
            break;
        }
        rows.extend(batch);
    }
    drop(stream);
    done.await.unwrap();
    QueryResult { columns, rows }
}

async fn handle(req: Request) -> Result {
    if !matches!((req.method(), req.path()), ("GET", "/") | ("POST", "/set")) {
        return Ok(Response::text("Use GET / or POST /set with a UTF-8 body").status(404));
    }
    let value = match (req.method(), req.text()) {
        ("GET", _) => None,
        (_, Ok(v)) if v.len() <= 1024 => Some(v.to_string()),
        _ => return Ok(Response::text("Expected at most 1024 UTF-8 bytes").status(400)),
    };
    let db = Connection::open_async("default".into()).await.unwrap();
    query(
        &db,
        "CREATE TABLE IF NOT EXISTS state(id INTEGER PRIMARY KEY CHECK(id=1), value TEXT NOT NULL)",
        vec![],
    )
    .await;
    if let Some(value) = value {
        query(
            &db,
            "INSERT INTO state VALUES (1, ?) ON CONFLICT(id) DO UPDATE SET value=excluded.value",
            vec![Value::Text(value.clone())],
        )
        .await;
        data().write("example.txt", value.as_bytes()).await.unwrap();
        kv::set("example", &value).unwrap();
        if req.query("rollback") == Some("1") {
            // Returning an HTTP 500 would COMMIT. A guest error aborts.
            return Err(Error::new("intentional example rollback"));
        }
    }
    let result = query(&db, "SELECT value FROM state WHERE id=1", vec![]).await;
    let sql = result.rows.first().map(|row| match &row.values[0] {
        Value::Text(v) => v.clone(),
        _ => unreachable!(),
    });
    let file = data().read("example.txt").await.unwrap().map(|bytes| String::from_utf8(bytes).unwrap());
    let kv = kv::get("example").unwrap().map(|v| String::from_utf8(v).unwrap());
    Ok(Response::json(serde_json::json!({"sql": sql, "file": file, "kv": kv}).to_string()))
}
