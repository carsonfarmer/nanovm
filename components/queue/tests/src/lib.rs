//! A minimal app over the queue component, driven by crates/nanovm/tests/components.rs.
//! Probes panic on violated expectations.
mod bindings {
    wit_bindgen::generate!({ path: "../wit", world: "client", generate_all });
}
use bindings::nanovm::queue::log::{Error, Message, Topic};
use serde_json::{Value, json};
use util::wasip3::http::types::{ErrorCode, Request, Response};
util::wasip3::http::service::export!(App);
struct App;

fn messages(list: &[Message]) -> Value {
    list.iter()
        .map(|m| json!({"offset": m.offset, "ts": m.timestamp_ms, "payload": String::from_utf8_lossy(&m.payload)}))
        .collect()
}

fn fail(e: Error) -> Result<Response, ErrorCode> {
    util::json(409, json!({"error": format!("{e:?}")}).to_string())
}

impl util::wasip3::exports::http::handler::Guest for App {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let req = util::read_req(request).await;
        let topic = Topic::open(req.q("topic").unwrap_or("jobs").into())
            .await
            .expect("open");
        let consumer = || req.q("consumer").unwrap_or("worker").to_string();
        let out = match req.path.as_str() {
            // Each line of the body is one message.
            "/append" => {
                let body = String::from_utf8(req.body.clone()).expect("UTF-8");
                let batch = body.split('\n').map(|l| l.as_bytes().to_vec()).collect();
                match topic.append(batch).await {
                    Ok(first) => json!({"first": first}),
                    Err(e) => return fail(e),
                }
            }
            "/bulk" => {
                let (count, size) = (req.q_u64("count", 1), req.q_u64("size", 100) as usize);
                let per = req.q_u64("batch", 1).max(1);
                let mut first = None;
                let mut n = 0;
                while n < count {
                    let k = per.min(count - n);
                    let batch = (0..k).map(|i| format!("{:0size$}", n + i).into_bytes()).collect();
                    let at = topic.append(batch).await.expect("bulk append");
                    first.get_or_insert(at);
                    n += k;
                }
                json!({"first": first})
            }
            "/read" => {
                let got = topic.read(req.q_u64("offset", 0), req.q_u64("max", 100) as u32).await;
                json!({"messages": messages(&got.expect("read"))})
            }
            "/bounds" => {
                let (oldest, next) = topic.bounds().await.expect("bounds");
                json!({"oldest": oldest, "next": next})
            }
            "/next" => {
                let got = topic
                    .next(consumer(), req.q_u64("max", 100) as u32)
                    .await
                    .expect("next");
                if req.q("fail") == Some("1") {
                    // A guest error rolls back the position: these come again.
                    return Err(ErrorCode::InternalError(Some("processing failed".into())));
                }
                json!({"messages": messages(&got)})
            }
            "/position" => json!({"position": topic.position(consumer()).await.expect("position")}),
            "/seek" => match topic.seek(consumer(), req.q_u64("offset", 0)).await {
                Ok(()) => json!({}),
                Err(e) => return fail(e),
            },
            "/consumers" => json!({"consumers": topic.consumers().await.expect("consumers")}),
            "/remove" => {
                topic.remove_consumer(consumer()).await.expect("remove");
                json!({})
            }
            "/trim" => match topic.trim(req.q_u64("before", 0)).await {
                Ok(()) => json!({}),
                Err(e) => return fail(e),
            },
            "/trap" => {
                topic.append(vec![b"never committed".to_vec()]).await.expect("append");
                panic!("rollback probe");
            }
            // Appends `size`-byte messages until the quota refuses one, then
            // checks the refusal changed nothing.
            "/fill" => {
                let size = req.q_u64("size", 64 << 10) as usize;
                let before = topic.bounds().await.expect("bounds");
                let mut appended = 0u64;
                let err = loop {
                    match topic.append(vec![vec![b'x'; size]]).await {
                        Ok(_) => appended += 1,
                        Err(e) => break e,
                    }
                };
                assert!(matches!(err, Error::Full), "{err:?}");
                let after = topic.bounds().await.expect("bounds");
                assert_eq!(after.1, before.1 + appended, "a refused append changed the log");
                if appended > 0 {
                    let last = topic.read(after.1 - 1, 1).await.expect("read last");
                    assert_eq!(last[0].payload.len(), size);
                }
                // A refused consumer file leaves nothing either.
                json!({"appended": appended, "error": format!("{err:?}"), "next": after.1})
            }
            "/contracts" => {
                for bad in ["", "../x", "a/b", &"n".repeat(65)] {
                    assert!(
                        matches!(Topic::open(bad.into()).await, Err(Error::InvalidName)),
                        "{bad}"
                    );
                }
                let t = Topic::open("contracts".into()).await.expect("open");
                assert_eq!(t.bounds().await.expect("bounds"), (0, 0));
                assert!(t.read(0, 10).await.expect("empty read").is_empty());
                assert!(matches!(t.append(vec![]).await, Err(Error::InvalidArgument(_))));
                assert!(matches!(
                    t.append(vec![vec![0; (1 << 20) + 1]]).await,
                    Err(Error::InvalidArgument(_))
                ));
                assert!(matches!(t.read(0, 0).await, Err(Error::InvalidArgument(_))));
                assert!(matches!(t.read(0, 1025).await, Err(Error::InvalidArgument(_))));
                assert!(matches!(t.seek("c".into(), 1).await, Err(Error::InvalidArgument(_))));
                assert!(matches!(t.trim(1).await, Err(Error::InvalidArgument(_))));
                assert!(matches!(t.position("bad/name".into()).await, Err(Error::InvalidName)));
                // Empty and binary messages round-trip; two handles share state.
                let other = Topic::open("contracts".into()).await.expect("second handle");
                assert_eq!(t.append(vec![vec![], vec![0, 255, 10]]).await.expect("append"), 0);
                assert_eq!(other.append(vec![b"c".to_vec()]).await.expect("append"), 2);
                let got = t.read(0, 10).await.expect("read");
                let payloads: Vec<_> = got.iter().map(|m| m.payload.clone()).collect();
                assert_eq!(payloads, vec![vec![], vec![0, 255, 10], b"c".to_vec()]);
                assert!(got.iter().all(|m| m.timestamp_ms > 1_600_000_000_000));
                assert_eq!(t.read(1, 1).await.expect("one").len(), 1);
                assert!(t.read(3, 10).await.expect("at end").is_empty());
                // A read stops near 1 MiB of payload, but returns at least one message.
                t.append(vec![vec![1; 700 << 10], vec![2; 700 << 10]])
                    .await
                    .expect("big");
                assert_eq!(t.read(3, 10).await.expect("budget").len(), 1);
                assert_eq!(t.read(4, 10).await.expect("budget").len(), 1);
                assert_eq!(t.next("c1".into(), 2).await.expect("next").len(), 2);
                assert_eq!(t.position("c1".into()).await.expect("position"), 2);
                assert_eq!(other.position("c2".into()).await.expect("fresh consumer"), 0);
                drop(other);
                t.remove_consumer("c1".into()).await.expect("remove");
                t.remove_consumer("c1".into()).await.expect("remove absent");
                assert!(t.consumers().await.expect("consumers").is_empty());
                json!("ok")
            }
            _ => return util::text(404, "no such route"),
        };
        util::json(200, out.to_string())
    }
}
