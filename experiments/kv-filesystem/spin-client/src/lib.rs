//! Executable contract probes; intentionally panic on violated expectations.
mod bindings {
    wit_bindgen::generate!({ path: "../spin-wit", world: "client", generate_all });
}
use bindings::spin::key_value::key_value::Store;
use util::wasip3::http::types::{ErrorCode, Request, Response};
util::wasip3::http::service::export!(Client);
struct Client;
impl util::wasip3::exports::http::handler::Guest for Client {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let req = util::read_req(request).await;
        let store = Store::open("default".into()).await.expect("open");
        match req.path.as_str() {
            "/set" => {
                store.set("key".into(), b"committed".to_vec()).await.expect("set");
            }
            "/get" => {
                return util::text(
                    200,
                    String::from_utf8(store.get("key".into()).await.expect("get").expect("present")).expect("UTF-8"),
                );
            }
            "/trap" => {
                store.set("key".into(), b"rolled-back".to_vec()).await.expect("set");
                panic!("rollback probe");
            }
            "/contracts" => {
                assert!(Store::open("../bad".into()).await.is_err());
                assert!(store.set("".into(), vec![]).await.is_err());
                assert!(store.set("huge".into(), vec![1; 1024 * 1024 + 1]).await.is_err());
                assert_eq!(store.get("missing".into()).await.expect("missing"), None);
                assert!(!store.exists("missing".into()).await.expect("exists"));
                store.delete("missing".into()).await.expect("delete absent");
                store.set("empty-value".into(), vec![]).await.expect("empty set");
                assert_eq!(store.get("empty-value".into()).await.expect("empty get"), Some(vec![]));
                store.delete("empty-value".into()).await.expect("empty delete");
                store
                    .set("é/../key".into(), b"unicode".to_vec())
                    .await
                    .expect("set encoded");
                assert_eq!(
                    store.get("é/../key".into()).await.expect("get encoded"),
                    Some(b"unicode".to_vec())
                );
                store.delete("é/../key".into()).await.expect("delete");
                assert!(!store.exists("é/../key".into()).await.expect("deleted"));
                // Both APIs see the same native KV files, in the same request.
                util::write_file("/data", ".kv/default/a2V5", b"direct".to_vec())
                    .await
                    .expect("file write");
                assert_eq!(
                    store.get("key".into()).await.expect("file -> KV"),
                    Some(b"direct".to_vec())
                );
                let old = util::open("/data", ".kv/default/a2V5", util::OFlags::empty(), util::FdFlags::READ)
                    .await
                    .expect("old descriptor");
                store.set("key".into(), b"replaced".to_vec()).await.expect("replace");
                let (bytes, done) = old.read_via_stream(0);
                assert_eq!(bytes.collect().await, b"direct");
                done.await.expect("old file completion");
                assert_eq!(
                    util::read_file("/data", ".kv/default/a2V5").await.expect("KV -> file"),
                    b"replaced"
                );
                let native = util::kv::store::open("").expect("native");
                assert_eq!(native.get("key").expect("native get"), Some(b"replaced".to_vec()));
                native.set("key", b"native").expect("native set");
                assert_eq!(
                    store.get("key".into()).await.expect("native -> Spin"),
                    Some(b"native".to_vec())
                );
            }
            "/populate" => {
                let bulk = Store::open("bulk".into()).await.expect("bulk");
                for n in 0..200 {
                    bulk.set(format!("key-{n:03}"), vec![n as u8]).await.expect("populate");
                }
            }
            "/list" => {
                let bulk = Store::open("bulk".into()).await.expect("bulk");
                let (mut keys, done) = bulk.get_keys().await;
                drop(bulk); // Listing owns its lifetime, not a borrowed store.
                let mut got = Vec::new();
                while let Some(key) = keys.next().await {
                    got.push(key);
                }
                drop(keys);
                done.await.expect("listing completion");
                got.sort();
                assert_eq!(got, (0..200).map(|n| format!("key-{n:03}")).collect::<Vec<_>>());
            }
            "/cancel" => {
                let bulk = Store::open("bulk".into()).await.expect("bulk");
                for _ in 0..100 {
                    let (mut keys, done) = bulk.get_keys().await;
                    assert!(keys.next().await.is_some());
                    drop(keys);
                    done.await.expect("cancel completion");
                }
            }
            "/pressure" => {
                use futures::FutureExt;
                use std::future::IntoFuture;
                let bulk = Store::open("bulk".into()).await.expect("bulk");
                let mut held = Vec::new();
                for _ in 0..30 {
                    let (mut keys, done) = bulk.get_keys().await;
                    let mut done = Box::pin(done.into_future());
                    assert!(keys.next().await.is_some());
                    assert!(
                        (&mut done).now_or_never().is_none(),
                        "producer finished despite backpressure"
                    );
                    held.push((keys, done));
                }
                let (mut keys, done) = bulk.get_keys().await;
                assert!(keys.next().await.is_none());
                drop(keys);
                assert!(matches!(
                    done.await,
                    Err(bindings::spin::key_value::key_value::Error::StoreTableFull)
                ));
                for (keys, done) in held {
                    drop(keys);
                    done.await.expect("release held listing");
                }
                // Cancel before consuming even one entry.
                for _ in 0..100 {
                    let (keys, done) = bulk.get_keys().await;
                    drop(keys);
                    done.await.expect("unread cancellation");
                }
                let (mut keys, done) = bulk.get_keys().await;
                assert!(keys.next().await.is_some());
                drop(keys);
                done.await.expect("capacity recovered");
            }
            "/concurrent" => {
                let a = vec![b'a'; 4097];
                let b = vec![b'b'; 8193];
                let (left, right) =
                    futures::join!(store.set("race".into(), a.clone()), store.set("race".into(), b.clone()));
                left.expect("left write");
                right.expect("right write");
                let got = store.get("race".into()).await.expect("race read").expect("present");
                assert!(got == a || got == b, "concurrent writes produced torn data");
            }
            "/empty" => {
                let empty = Store::open("empty".into()).await.expect("empty");
                let (mut keys, done) = empty.get_keys().await;
                assert!(keys.next().await.is_none());
                drop(keys);
                done.await.expect("empty completion");
            }
            "/listing-error" => {
                let empty = Store::open("vanished".into()).await.expect("vanished");
                util::mount("/data")
                    .expect("mount")
                    .remove_directory_at(".kv/vanished".into())
                    .await
                    .expect("remove dir");
                let (mut keys, done) = empty.get_keys().await;
                assert!(keys.next().await.is_none());
                drop(keys);
                assert!(done.await.is_err(), "completion must carry directory error");
            }
            "/quota" => {
                store.set("original".into(), b"keep".to_vec()).await.expect("original");
                for n in 0..20 {
                    if store.set(format!("fill-{n}"), vec![7; 1024 * 1024]).await.is_err() {
                        assert!(store.set("original".into(), vec![8; 1024 * 1024]).await.is_err());
                        assert_eq!(
                            store.get("original".into()).await.expect("old value"),
                            Some(b"keep".to_vec())
                        );
                        return util::text(200, "quota refused");
                    }
                }
                panic!("quota was bypassed");
            }
            _ => return util::text(404, "unknown probe"),
        }
        util::text(200, "ok")
    }
}
