//! Executable contract probes; intentionally panic on violated expectations.
mod bindings {
    wit_bindgen::generate!({ path: "../wit", world: "client", generate_all });
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
            "/fault" => {
                util::write_file("/data", "mixed", b"uncommitted".to_vec())
                    .await
                    .expect("mixed write");
                store
                    .set("key".into(), vec![b'f'; 1024 * 1024])
                    .await
                    .expect("fault write");
            }
            "/repeat-seed" => store.set("repeat".into(), b"seed".to_vec()).await.expect("seed"),
            "/repeat" => {
                for n in 0..1000 {
                    let value = vec![(n % 251) as u8; if n % 2 == 0 { 4097 } else { 8193 }];
                    store.set("repeat".into(), value.clone()).await.expect("repeat set");
                    assert_eq!(store.get("repeat".into()).await.expect("repeat get"), Some(value));
                }
                util::write_file("/data", "repeat-ready", vec![]).await.expect("ready");
                util::wasip3::clocks::monotonic_clock::wait_for(200_000_000).await;
                if req.q("trap") == Some("1") {
                    panic!("repeated replacement rollback");
                }
            }
            "/repeat-get" => {
                return util::respond(
                    200,
                    "application/octet-stream",
                    store.get("repeat".into()).await.expect("get").expect("value"),
                );
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
            "/held-completions" => {
                let empty = Store::open("empty".into()).await.expect("empty");
                let mut held = Vec::new();
                for _ in 0..30 {
                    let (mut keys, done) = empty.get_keys().await;
                    assert!(keys.next().await.is_none());
                    drop(keys);
                    held.push(done);
                }
                let (mut keys, done) = empty.get_keys().await;
                assert!(keys.next().await.is_none());
                drop(keys);
                assert!(matches!(
                    done.await,
                    Err(bindings::spin::key_value::key_value::Error::StoreTableFull)
                ));
                drop(held);
                // Endpoint closure wakes the producers; let their tasks unwind.
                util::wasip3::clocks::monotonic_clock::wait_for(1_000_000).await;
                let (mut keys, done) = empty.get_keys().await;
                assert!(keys.next().await.is_none());
                drop(keys);
                done.await.expect("completion slots released");
            }
            "/collision" => {
                let dir = util::mount("/data").expect("data");
                // This route runs on a fresh instance, whose temporary counter is zero.
                dir.create_directory_at(".kv".into()).await.expect("kv root");
                dir.create_directory_at(".kv/default".into()).await.expect("bucket");
                util::write_file("/data", ".kv/default/.spin-write-0", b"sentinel".to_vec())
                    .await
                    .expect("plant");
                store
                    .set("collision".into(), b"new".to_vec())
                    .await
                    .expect("collision retry");
                assert_eq!(
                    util::read_file("/data", ".kv/default/.spin-write-0").await.expect("sentinel"),
                    b"sentinel"
                );
                dir.unlink_file_at(".kv/default/.spin-write-0".into())
                    .await
                    .expect("remove sentinel");
            }
            "/cancel-write" => {
                use futures::future::{Either, select};
                store.set("cancel".into(), b"old".to_vec()).await.expect("old value");
                let started = async {
                    loop {
                        if util::read_file("/data", "kv-paused").await.is_ok() {
                            break;
                        }
                        util::wasip3::clocks::monotonic_clock::wait_for(1_000_000).await;
                    }
                };
                match select(
                    Box::pin(store.set("cancel".into(), vec![b'n'; 1024 * 1024])),
                    Box::pin(started),
                )
                .await
                {
                    Either::Right(((), pending)) => drop(pending),
                    Either::Left(_) => panic!("test did not cancel a started write"),
                }
                util::wasip3::clocks::monotonic_clock::wait_for(150_000_000).await;
                let value = store
                    .get("cancel".into())
                    .await
                    .expect("read after cancellation")
                    .expect("value");
                assert!(value == b"old" || value == vec![b'n'; 1024 * 1024], "torn value");
                let dir = util::open("/data", ".kv/default", util::OFlags::DIRECTORY, util::FdFlags::READ)
                    .await
                    .expect("kv directory");
                let (mut entries, done) = dir.read_directory();
                while let Some(entry) = entries.next().await {
                    assert!(
                        !entry.name.starts_with(".spin-write-"),
                        "cancelled write left a temporary file"
                    );
                }
                drop(entries);
                done.await.expect("list completion");
            }
            "/empty" => {
                let empty = Store::open("read-only".into()).await.expect("empty");
                assert!(empty.get("absent".into()).await.unwrap().is_none());
                assert!(!empty.exists("absent".into()).await.unwrap());
                empty.delete("absent".into()).await.unwrap();
                let (mut keys, done) = empty.get_keys().await;
                assert!(keys.next().await.is_none());
                drop(keys);
                done.await.expect("empty completion");
                assert!(matches!(
                    util::open("/data", ".kv/read-only", util::OFlags::DIRECTORY, util::FdFlags::READ).await,
                    Err(util::wasip3::filesystem::types::ErrorCode::NoEntry)
                ), "reading a new bucket must not create it");
            }
            "/listing-error" => {
                let empty = Store::open("vanished".into()).await.expect("vanished");
                util::write_file("/data", ".kv/vanished", b"not a directory".to_vec())
                    .await
                    .expect("obstruct bucket");
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
