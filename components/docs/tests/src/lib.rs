//! A minimal app over the document store, driven by crates/nanovm/tests/components.rs.
//! Probes panic on violated expectations.
mod bindings {
    wit_bindgen::generate!({ path: "../wit", world: "client", generate_all });
}
use bindings::nanovm::docs::store::{Bound, Collection, Document, Error, Query};
use serde_json::{Value, json};
use util::wasip3::http::types::{ErrorCode, Request, Response};
util::wasip3::http::service::export!(App);
struct App;

fn docs(list: &[Document]) -> Value {
    list.iter()
        .map(|d| json!({"id": d.id, "body": serde_json::from_str::<Value>(&d.body).unwrap()}))
        .collect()
}

fn fail(e: Error) -> Result<Response, ErrorCode> {
    util::json(409, json!({"error": format!("{e:?}")}).to_string())
}

fn bound(req: &util::Req, inc: &str, exc: &str) -> Bound {
    match (req.q(inc), req.q(exc)) {
        (Some(v), _) => Bound::Included(v.into()),
        (None, Some(v)) => Bound::Excluded(v.into()),
        (None, None) => Bound::Unbounded,
    }
}

impl util::wasip3::exports::http::handler::Guest for App {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let req = util::read_req(request).await;
        let coll = Collection::open(req.q("coll").unwrap_or("people").into())
            .await
            .expect("open");
        let id = || req.q("id").unwrap_or("").to_string();
        let body = || String::from_utf8(req.body.clone()).expect("UTF-8");
        let out = match (req.method.as_str(), req.path.as_str()) {
            ("PUT", "/doc") => match coll.put(id(), body()).await {
                Ok(()) => json!({}),
                Err(e) => return fail(e),
            },
            ("GET", "/doc") => match coll.get(id()).await.expect("get") {
                Some(b) => serde_json::from_str::<Value>(&b).unwrap(),
                None => return util::text(404, "no such document"),
            },
            ("DELETE", "/doc") => json!({"existed": coll.delete(id()).await.expect("delete")}),
            ("GET", "/list") => {
                let got = coll
                    .scan(req.q("after").map(String::from), req.q_u64("limit", 100) as u32)
                    .await;
                docs(&got.expect("list"))
            }
            ("POST", "/index") => match coll
                .create_index(req.q("field").unwrap().into(), req.q("unique") == Some("1"))
                .await
            {
                Ok(()) => json!({}),
                Err(e) => return fail(e),
            },
            ("DELETE", "/index") => match coll.drop_index(req.q("field").unwrap().into()).await {
                Ok(()) => json!({}),
                Err(e) => return fail(e),
            },
            ("POST", "/text-index") => match coll.create_text_index(req.q("field").unwrap().into()).await {
                Ok(()) => json!({}),
                Err(e) => return fail(e),
            },
            ("GET", "/search") => {
                let (field, q) = (
                    req.q("field").unwrap().to_string(),
                    req.q("q").unwrap_or("").to_string(),
                );
                match coll.search(field, q, req.q_u64("limit", 10) as u32).await {
                    Ok(hits) => hits.iter().map(|h| json!([h.document.id, h.score])).collect(),
                    Err(e) => return fail(e),
                }
            }
            ("POST", "/vector-index") => {
                let (field, dims) = (req.q("field").unwrap().into(), req.q_u64("dims", 3) as u32);
                match coll.create_vector_index(field, dims).await {
                    Ok(()) => json!({}),
                    Err(e) => return fail(e),
                }
            }
            ("GET", "/nearest") => {
                let v: Vec<f32> = req.q("v").unwrap().split(',').map(|x| x.parse().unwrap()).collect();
                match coll
                    .nearest(req.q("field").unwrap().into(), v, req.q_u64("k", 10) as u32)
                    .await
                {
                    Ok(hits) => hits.iter().map(|h| json!([h.document.id, h.score])).collect(),
                    Err(e) => return fail(e),
                }
            }
            // `count` documents {"emb": [...]} of `dims` pseudo-random numbers.
            ("POST", "/bulk-vectors") => {
                let (count, dims) = (req.q_u64("count", 1), req.q_u64("dims", 64));
                let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
                for n in 0..count {
                    let v: Vec<f64> = (0..dims)
                        .map(|_| {
                            x ^= x << 13;
                            x ^= x >> 7;
                            x ^= x << 17;
                            (x % 2000) as f64 / 1000.0 - 1.0
                        })
                        .collect();
                    coll.put(format!("v{n:06}"), json!({"emb": v}).to_string())
                        .await
                        .expect("put");
                }
                json!({})
            }
            ("GET", "/indexes") => {
                let list = coll.indexes().await.expect("indexes");
                list.iter()
                    .map(|ix| json!({"field": ix.field, "unique": ix.unique}))
                    .collect()
            }
            ("GET", "/query") => {
                let (lower, upper) = match req.q("eq") {
                    Some(v) => (Bound::Included(v.into()), Bound::Included(v.into())),
                    None => (bound(&req, "gte", "gt"), bound(&req, "lte", "lt")),
                };
                let q = Query {
                    field: req.q("field").unwrap().into(),
                    lower,
                    upper,
                    after: req.q("after").map(String::from),
                    limit: req.q_u64("limit", 100) as u32,
                };
                match coll.query(q).await {
                    Ok(page) => {
                        json!({"ids": page.documents.iter().map(|d| &d.id).collect::<Vec<_>>(), "cursor": page.cursor})
                    }
                    Err(e) => return fail(e),
                }
            }
            ("GET", "/stats") => {
                let s = coll.stats().await.expect("stats");
                json!({"documents": s.documents, "live": s.live_bytes, "log": s.log_bytes})
            }
            ("POST", "/compact") => {
                coll.compact().await.expect("compact");
                json!({})
            }
            // Puts `count` documents {"n", "tag", "pad"} with ids `<prefix><n>`.
            ("POST", "/bulk") => {
                let (count, size) = (req.q_u64("count", 1), req.q_u64("size", 0) as usize);
                let prefix = req.q("prefix").unwrap_or("d");
                for n in 0..count {
                    let doc = json!({"n": n, "tag": format!("t{}", n % 10), "pad": "x".repeat(size)});
                    coll.put(format!("{prefix}{n:06}"), doc.to_string())
                        .await
                        .expect("bulk put");
                }
                json!({})
            }
            // A failed request: its changes roll back.
            ("POST", "/fail") => {
                coll.put(id(), body()).await.expect("put");
                if let Some(field) = req.q("field") {
                    coll.create_index(field.into(), false).await.expect("index");
                }
                return Err(ErrorCode::InternalError(Some("rollback probe".into())));
            }
            ("POST", "/trap") => {
                coll.delete(id()).await.expect("delete");
                panic!("rollback probe");
            }
            // Puts `size`-byte documents until the quota refuses one, then
            // checks the refusal changed nothing.
            ("POST", "/fill") => {
                // With `fine=1`, then smaller and smaller ones, to the last byte.
                let mut size = req.q_u64("size", 64 << 10) as usize;
                let before = coll.stats().await.expect("stats");
                let mut n = 0u64;
                let err = loop {
                    let doc = json!({"n": n, "pad": "y".repeat(size)}).to_string();
                    match coll.put(format!("fill{n}"), doc).await {
                        Ok(()) => n += 1,
                        Err(e) if req.q("fine") == Some("1") && size > 1 => {
                            assert!(matches!(e, Error::Full), "{e:?}");
                            size /= 2;
                        }
                        Err(e) => break e,
                    }
                };
                assert!(matches!(err, Error::Full), "{err:?}");
                let after = coll.stats().await.expect("stats");
                assert_eq!(
                    after.documents,
                    before.documents + n,
                    "a refused put changed the collection"
                );
                assert!(coll.get(format!("fill{n}")).await.expect("get").is_none());
                json!({"stored": n, "error": format!("{err:?}"), "log": after.log_bytes})
            }
            ("GET", "/contracts") => {
                for bad in ["", "../x", "a/b", &"n".repeat(65)] {
                    assert!(
                        matches!(Collection::open(bad.into()).await, Err(Error::InvalidName)),
                        "{bad}"
                    );
                }
                let c = Collection::open("contracts".into()).await.expect("open");
                assert_eq!(c.get("x".into()).await.expect("absent"), None);
                assert!(!c.delete("x".into()).await.expect("delete absent"));
                assert!(c.scan(None, 10).await.expect("empty list").is_empty());
                assert!(matches!(
                    c.put("".into(), "{}".into()).await,
                    Err(Error::InvalidArgument(_))
                ));
                assert!(matches!(
                    c.put("x".repeat(257), "{}".into()).await,
                    Err(Error::InvalidArgument(_))
                ));
                for bad in ["[1]", "3", "{", "\"s\""] {
                    assert!(
                        matches!(c.put("x".into(), bad.into()).await, Err(Error::InvalidJson(_))),
                        "{bad}"
                    );
                }
                let big = json!({"p": "z".repeat(1 << 20)}).to_string();
                assert!(matches!(c.put("x".into(), big).await, Err(Error::InvalidArgument(_))));
                assert!(matches!(c.scan(None, 0).await, Err(Error::InvalidArgument(_))));
                for bad in ["", "a..b", ".a", &"f".repeat(129)] {
                    assert!(matches!(
                        c.create_index(bad.into(), false).await,
                        Err(Error::InvalidArgument(_))
                    ));
                }
                let q = |field: &str, lower: Bound| Query {
                    field: field.into(),
                    lower,
                    upper: Bound::Unbounded,
                    after: None,
                    limit: 10,
                };
                assert!(matches!(
                    c.query(q("none", Bound::Unbounded)).await,
                    Err(Error::NoSuchIndex(_))
                ));
                c.create_index("a.b".into(), false).await.expect("index");
                c.create_index("a.b".into(), false).await.expect("same index again");
                assert!(matches!(
                    c.create_index("a.b".into(), true).await,
                    Err(Error::InvalidArgument(_))
                ));
                assert!(matches!(
                    c.query(q("a.b", Bound::Included("{}".into()))).await,
                    Err(Error::InvalidJson(_))
                ));
                // Order across types, nested paths, arrays and ids with any characters.
                for (id, v) in [
                    ("s", json!("str")),
                    ("n2", json!(2)),
                    ("n10", json!(10.5)),
                    ("t", json!(true)),
                    ("f", json!(false)),
                    ("z", Value::Null),
                    ("arr", json!([3, "str", [9]])),
                    ("obj", json!({"c": 1})),
                    ("é/../ ✓", json!(-1)),
                ] {
                    c.put(id.into(), json!({"a": {"b": v}}).to_string()).await.expect("put");
                }
                c.put("missing".into(), "{}".into()).await.expect("no field");
                let ids = |page: bindings::nanovm::docs::store::Page| {
                    page.documents.into_iter().map(|d| d.id).collect::<Vec<_>>()
                };
                let all = ids(c.query(q("a.b", Bound::Unbounded)).await.expect("all"));
                assert_eq!(all, ["z", "f", "t", "é/../ ✓", "n2", "arr", "n10", "arr", "s"]);
                // Pagination by cursor reaches the same order.
                let mut paged = Vec::new();
                let mut after = None;
                loop {
                    let page = c
                        .query(Query {
                            after: after.clone(),
                            limit: 2,
                            ..q("a.b", Bound::Unbounded)
                        })
                        .await
                        .expect("page");
                    after = page.cursor.clone();
                    paged.extend(ids(page));
                    if after.is_none() {
                        break;
                    }
                }
                assert_eq!(paged, all);
                let range = Query {
                    upper: Bound::Excluded("10.5".into()),
                    ..q("a.b", Bound::Excluded("-1".into()))
                };
                assert_eq!(ids(c.query(range).await.expect("range")), ["n2", "arr"]);
                assert_eq!(
                    c.get("é/../ ✓".into()).await.expect("get"),
                    Some(r#"{"a":{"b":-1}}"#.into())
                );
                let listed: Vec<_> = c
                    .scan(Some("n10".into()), 2)
                    .await
                    .expect("list")
                    .into_iter()
                    .map(|d| d.id)
                    .collect();
                assert_eq!(listed, ["n2", "obj"]);
                c.drop_index("a.b".into()).await.expect("drop");
                assert!(matches!(c.drop_index("a.b".into()).await, Err(Error::NoSuchIndex(_))));
                assert!(c.indexes().await.expect("indexes").is_empty());
                json!("ok")
            }
            _ => return util::text(404, "no such route"),
        };
        util::json(200, out.to_string())
    }
}
