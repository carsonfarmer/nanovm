//! HTTP client with no SQLite engine: all SQL crosses the composed WIT boundary.
use std::cell::RefCell;
use util::wasip3::http::types::{ErrorCode, Request, Response};
mod bindings {
    wit_bindgen::generate!({ path: "../wit", world: "client", generate_all });
}
use bindings::spin::sqlite::sqlite::{Connection, Error, QueryResult, Value};
util::wasip3::http::service::export!(Client);
struct Client;
thread_local! { static CONNECTION: RefCell<Option<Connection>> = const { RefCell::new(None) }; }

async fn query(db: &Connection, sql: String, parameters: Vec<Value>) -> Result<QueryResult, Error> {
    let (columns, mut stream, done) = db.execute_async(sql, parameters).await?;
    let mut rows = Vec::new();
    loop {
        let (_, batch) = stream.read(Vec::with_capacity(16)).await;
        if batch.is_empty() {
            break;
        }
        rows.extend(batch);
    }
    drop(stream);
    done.await?;
    Ok(QueryResult { columns, rows })
}

impl util::wasip3::exports::http::handler::Guest for Client {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let req = util::read_req(request).await;
        let cached = CONNECTION.with(|c| c.borrow_mut().take());
        let reused = cached.is_some();
        let db = match cached {
            Some(db) => db,
            None => Connection::open_async("default".into()).await.unwrap(),
        };
        assert!(matches!(
            Connection::open_async("other".into()).await,
            Err(Error::NoSuchDatabase)
        ));
        assert!(matches!(
            Connection::open_async("default".into()).await,
            Err(Error::InvalidConnection)
        ));
        query(
            &db,
            "CREATE TABLE IF NOT EXISTS ledger(id INTEGER PRIMARY KEY, value TEXT)".into(),
            vec![],
        )
        .await
        .unwrap();
        // Verify all canonical-ABI value variants with asymmetric values.
        let typed = query(
            &db,
            "SELECT ? AS i, ? AS r, ? AS t, ? AS b, ? AS n".into(),
            vec![
                Value::Integer(-42),
                Value::Real(1.25),
                Value::Text("héllo".into()),
                Value::Blob(vec![0, 255, 7]),
                Value::Null,
            ],
        )
        .await
        .unwrap();
        assert_eq!(typed.columns, ["i", "r", "t", "b", "n"]);
        assert!(
            matches!(typed.rows[0].values.as_slice(), [Value::Integer(-42), Value::Real(r), Value::Text(t), Value::Blob(b), Value::Null]
            if *r == 1.25 && t == "héllo" && b == &[0,255,7])
        );
        let many_sql = "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<41) SELECT x FROM n";
        let many = query(&db, many_sql.into(), vec![]).await.unwrap();
        assert_eq!(many.rows.len(), 41);
        for (i, row) in many.rows.iter().enumerate() {
            assert!(matches!(row.values[0], Value::Integer(n) if n == i as i64 + 1));
        }
        // Abandon an unread result, then issue another query on the same connection.
        drop(db.execute_async(many_sql.into(), vec![]).await.unwrap());
        assert!(matches!(Connection::open("default"), Err(Error::Io(_))));
        assert!(matches!(db.execute("SELECT 1", &[]), Err(Error::Io(_))));
        for sql in ["SELECT FROM", "SELECT 1; SELECT 2"] {
            assert!(matches!(query(&db, sql.into(), vec![]).await, Err(Error::Io(_))));
        }
        for sql in [
            "BEGIN",
            "SAVEPOINT x",
            "COMMIT",
            "ATTACH 'other.db' AS other",
            "PRAGMA journal_mode=WAL",
            "PRAGMA synchronous=OFF",
        ] {
            assert!(
                matches!(query(&db, sql.into(), vec![]).await, Err(Error::AccessDenied)),
                "{sql}"
            );
        }
        if let Some(value) = req.q("v") {
            query(
                &db,
                "INSERT INTO ledger(value) VALUES (?)".into(),
                vec![Value::Text(value.into())],
            )
            .await
            .unwrap();
            assert_eq!(db.changes_async().await, 1);
            assert!(db.last_insert_rowid_async().await > 0);
            util::write_file("/data", "marker", value.as_bytes().to_vec())
                .await
                .unwrap();
            util::kv::store::open("")
                .unwrap()
                .set("marker", value.as_bytes())
                .unwrap();
        }
        if req.path == "/full" {
            let result = query(
                &db,
                "INSERT INTO ledger(value) VALUES (zeroblob(2097152))".into(),
                vec![],
            )
            .await;
            assert!(result.is_err(), "must exceed the unchanged 1 MiB quota");
            return Err(ErrorCode::InternalError(Some(format!(
                "composed quota: {:?}",
                result.err()
            ))));
        }
        let rows = query(&db, "SELECT value FROM ledger ORDER BY id".into(), vec![])
            .await
            .unwrap()
            .rows
            .into_iter()
            .map(|r| match r.values.into_iter().next().unwrap() {
                Value::Text(s) => s,
                _ => panic!("not text"),
            })
            .collect::<Vec<_>>();
        let integrity = query(&db, "PRAGMA integrity_check".into(), vec![]).await.unwrap();
        assert!(matches!(&integrity.rows[0].values[0], Value::Text(s) if s == "ok"));
        let file = util::read_file("/data", "marker").await.unwrap_or_default();
        let key = util::kv::store::open("")
            .unwrap()
            .get("marker")
            .unwrap()
            .unwrap_or_default();
        if req.path == "/close" {
            drop(db);
            // A dropped resource must release the one-connection slot.
            drop(Connection::open_async("default".into()).await.unwrap());
        } else {
            CONNECTION.with(|c| *c.borrow_mut() = Some(db));
        }
        match req.q("then") {
            Some("error") => return Err(ErrorCode::InternalError(Some("composed rollback".into()))),
            Some("trap") => panic!("composed trap"),
            _ => (),
        }
        util::json(if req.q("then") == Some("status") { 507 } else { 200 }, serde_json::json!({
            "rows": rows, "file": String::from_utf8(file).unwrap(), "key": String::from_utf8(key).unwrap(), "reused": reused,
        }).to_string())
    }
}
