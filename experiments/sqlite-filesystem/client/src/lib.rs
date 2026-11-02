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
        let mut db = match cached {
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
        if req.path == "/nohooks" {
            assert!(query(&db, "SELECT nanovm_test_steps()".into(), vec![]).await.is_err());
        }
        if req.path == "/overlap" {
            use std::{
                future::{Future, poll_fn},
                pin::pin,
                task::Poll,
            };
            let mut first = pin!(db.execute_async("SELECT nanovm_test_yield()".into(), vec![]));
            let mut second = pin!(db.execute_async("SELECT nanovm_test_yielding()".into(), vec![]));
            let (mut a, mut b) = (None, None);
            let (a, b) = poll_fn(|cx| {
                if a.is_none() {
                    if let Poll::Ready(v) = first.as_mut().poll(cx) {
                        a = Some(v);
                    }
                }
                if b.is_none() {
                    if let Poll::Ready(v) = second.as_mut().poll(cx) {
                        b = Some(v);
                    }
                }
                if a.is_some() && b.is_some() {
                    Poll::Ready((a.take().unwrap(), b.take().unwrap()))
                } else {
                    Poll::Pending
                }
            })
            .await;
            let (_, mut rows, done) = a.unwrap();
            let (_, batch) = rows.read(Vec::with_capacity(1)).await;
            assert!(matches!(batch[0].values[0], Value::Integer(42)));
            drop(rows);
            done.await.unwrap();
            let (_, mut rows, done) = b.unwrap();
            let (_, batch) = rows.read(Vec::with_capacity(1)).await;
            assert!(
                matches!(batch[0].values[0], Value::Integer(0)),
                "SQL executions overlapped"
            );
            drop(rows);
            done.await.unwrap();
            assert!(query(&db, "SELECT 9".into(), vec![]).await.is_ok());
            return util::json(200, serde_json::json!({"scheduling": "serialized"}).to_string());
        }
        if req.path == "/cuts" {
            query(
                &db,
                "CREATE TABLE IF NOT EXISTS pages(id INTEGER PRIMARY KEY,value INTEGER,payload BLOB)".into(),
                vec![],
            )
            .await
            .unwrap();
            let op = req.q("op").unwrap_or("read");
            if op == "init" {
                query(&db, "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<16) INSERT INTO pages SELECT x,x*3,zeroblob(1024) FROM n".into(), vec![]).await.unwrap();
            }
            if op == "init" || op == "mutate" {
                let marker = if op == "init" { "before" } else { "after" };
                util::write_file("/data", "marker", marker.as_bytes().to_vec())
                    .await
                    .unwrap();
                util::kv::store::open("")
                    .unwrap()
                    .set("marker", marker.as_bytes())
                    .unwrap();
            }
            if op == "mutate" {
                query(
                    &db,
                    "SELECT nanovm_test_arm(?)".into(),
                    vec![Value::Integer(req.q_u64("cut", 0) as i64)],
                )
                .await
                .unwrap();
                for sql in [
                    "UPDATE pages SET value=value+7,payload=zeroblob(8192)",
                    "DELETE FROM pages WHERE id>12",
                    "INSERT INTO pages VALUES(17,51,zeroblob(3072))",
                ] {
                    query(&db, sql.into(), vec![]).await.unwrap();
                }
            }
            let steps = query(&db, "SELECT nanovm_test_steps()".into(), vec![]).await.unwrap();
            let steps = match steps.rows[0].values[0] {
                Value::Integer(n) => n,
                _ => panic!(),
            };
            query(&db, "SELECT nanovm_test_arm(0)".into(), vec![]).await.unwrap();
            let rows = query(
                &db,
                "SELECT id,value,length(payload) FROM pages ORDER BY id".into(),
                vec![],
            )
            .await
            .unwrap()
            .rows
            .into_iter()
            .map(|row| {
                row.values
                    .into_iter()
                    .map(|v| match v {
                        Value::Integer(n) => n,
                        _ => panic!(),
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
            let integrity = query(&db, "PRAGMA integrity_check".into(), vec![]).await.unwrap();
            assert!(matches!(&integrity.rows[0].values[0], Value::Text(s) if s == "ok"));
            let file = util::read_file("/data", "marker").await.unwrap();
            let key = util::kv::store::open("").unwrap().get("marker").unwrap().unwrap();
            CONNECTION.with(|c| *c.borrow_mut() = Some(db));
            return util::json(200, serde_json::json!({"rows":rows,"file":String::from_utf8(file).unwrap(),"key":String::from_utf8(key).unwrap(),"steps":steps}).to_string());
        }
        if req.path == "/limits" {
            let mut sql = "SELECT 1".to_string();
            sql.push_str(&" ".repeat(65536 - sql.len()));
            assert!(query(&db, sql.clone(), vec![]).await.is_ok());
            sql.push(' ');
            assert!(matches!(query(&db, sql, vec![]).await, Err(Error::Io(_))));
            assert!(
                query(&db, "SELECT ?128".into(), vec![Value::Integer(3); 128])
                    .await
                    .is_ok()
            );
            assert!(matches!(
                query(&db, "SELECT ?129".into(), vec![Value::Integer(3); 129]).await,
                Err(Error::Io(_))
            ));
            let length = query(&db, "SELECT length(?)".into(), vec![Value::Blob(vec![7; 1048576])])
                .await
                .unwrap();
            assert!(matches!(length.rows[0].values[0], Value::Integer(1048576)));
            assert!(matches!(
                query(&db, "SELECT length(?)".into(), vec![Value::Blob(vec![7; 1048577])]).await,
                Err(Error::Io(_))
            ));
            for sql in [
                "SELECT zeroblob(1048576)",
                "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<4097) SELECT x FROM n",
            ] {
                assert!(
                    matches!(query(&db, sql.into(), vec![]).await, Err(Error::Io(_))),
                    "result must be bounded"
                );
            }
            let rows = query(
                &db,
                "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<4096) SELECT x FROM n".into(),
                vec![],
            )
            .await
            .unwrap();
            assert_eq!(rows.rows.len(), 4096);
            for _ in 0..8 {
                let held = db
                    .execute_async("SELECT zeroblob(700000)".into(), vec![])
                    .await
                    .unwrap();
                assert!(matches!(
                    query(&db, "SELECT zeroblob(700000)".into(), vec![]).await,
                    Err(Error::Io(_))
                ));
                drop(held);
                util::wasip3::clocks::monotonic_clock::wait_for(1_000_000).await;
            }
            let held = db
                .execute_async("SELECT zeroblob(700000)".into(), vec![])
                .await
                .unwrap();
            drop(db);
            db = Connection::open_async("default".into()).await.unwrap();
            assert!(matches!(
                query(&db, "SELECT zeroblob(700000)".into(), vec![]).await,
                Err(Error::Io(_))
            ));
            drop(held);
            util::wasip3::clocks::monotonic_clock::wait_for(1_000_000).await;
            assert!(query(&db, "SELECT zeroblob(700000)".into(), vec![]).await.is_ok());
        }
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
        if req.path == "/write-result-limit" {
            // Even if the app catches the result, this write must not commit.
            let _ = query(
                &db,
                "INSERT INTO ledger(value) VALUES ('invisible') RETURNING zeroblob(1048576)".into(),
                vec![],
            )
            .await;
        }
        if req.path == "/constraint" {
            assert!(
                query(
                    &db,
                    "INSERT INTO ledger(id,value) VALUES (1,'duplicate')".into(),
                    vec![]
                )
                .await
                .is_err()
            );
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
            Some("sleep") => util::wasip3::clocks::monotonic_clock::wait_for(60_000_000_000).await,
            _ => (),
        }
        util::json(if req.q("then") == Some("status") { 507 } else { 200 }, serde_json::json!({
            "rows": rows, "file": String::from_utf8(file).unwrap(), "key": String::from_utf8(key).unwrap(), "reused": reused,
        }).to_string())
    }
}
