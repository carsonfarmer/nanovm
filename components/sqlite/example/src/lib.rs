//! A small app using SQL, files and KV in the same request transaction.
use util::wasip3::http::types::{ErrorCode, Request, Response};
mod bindings {
    wit_bindgen::generate!({ path: "../wit", world: "client", generate_all });
}
use bindings::spin::sqlite::sqlite::{Connection, QueryResult, Value};
util::wasip3::http::service::export!(Example);
struct Example;

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

impl util::wasip3::exports::http::handler::Guest for Example {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let req = util::read_req(request).await;
        if !matches!(
            (req.method.as_str(), req.path.as_str()),
            ("GET", "/") | ("POST", "/set")
        ) {
            return util::text(404, "Use GET / or POST /set with a UTF-8 body");
        }
        let value = if req.method == "POST" {
            match String::from_utf8(req.body.clone()) {
                Ok(v) if v.len() <= 1024 => Some(v),
                _ => return util::text(400, "Expected at most 1024 UTF-8 bytes"),
            }
        } else {
            None
        };
        let db = Connection::open_async("default".into()).await.unwrap();
        query(
            &db,
            "CREATE TABLE IF NOT EXISTS state(id INTEGER PRIMARY KEY CHECK(id=1), value TEXT NOT NULL)",
            vec![],
        )
        .await;
        let bucket = util::kv::Store::open("default".into()).await.unwrap();
        if let Some(value) = value {
            query(
                &db,
                "INSERT INTO state VALUES (1, ?) ON CONFLICT(id) DO UPDATE SET value=excluded.value",
                vec![Value::Text(value.clone())],
            )
            .await;
            util::write_file("/data", "example.txt", value.as_bytes().to_vec())
                .await
                .unwrap();
            bucket.set("example".into(), value.as_bytes().to_vec()).await.unwrap();
            if req.q("rollback") == Some("1") {
                // Returning an HTTP 500 would COMMIT. A guest error aborts.
                return Err(ErrorCode::InternalError(Some("intentional example rollback".into())));
            }
        }
        let result = query(&db, "SELECT value FROM state WHERE id=1", vec![]).await;
        let sql = result.rows.first().map(|row| match &row.values[0] {
            Value::Text(v) => v.clone(),
            _ => unreachable!(),
        });
        let file = match util::read_file("/data", "example.txt").await {
            Ok(bytes) => Some(String::from_utf8(bytes).unwrap()),
            Err(util::wasip3::filesystem::types::ErrorCode::NoEntry) => None,
            Err(e) => panic!("read example file: {e:?}"),
        };
        let kv = bucket.get("example".into()).await.unwrap().map(|v| String::from_utf8(v).unwrap());
        util::json(200, serde_json::json!({"sql": sql, "file": file, "kv": kv}).to_string())
    }
}
