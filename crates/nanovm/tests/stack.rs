//! The one-command local stack (tools/local-stack) comes up and serves the
//! full routed path: TXT record, tag, digest, node, jailed worker. Build the
//! guests first with `scripts/build-guests.sh`.

#[path = "../support/stack.rs"]
#[allow(dead_code)]
mod stack;

use std::path::Path;

#[tokio::test(flavor = "multi_thread")]
async fn local_stack_serves_the_routed_path() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("local-stack");
    let _ = std::fs::remove_dir_all(&dir);
    let s = stack::Stack::start(stack::Options {
        nanovm: env!("CARGO_BIN_EXE_nanovm").into(),
        dir,
        listen: "127.0.0.1:0".parse().unwrap(),
        tag_ttl_s: 0,
        registry: "127.0.0.1:0".parse().unwrap(),
    })
    .await
    .unwrap();
    let wasm = std::fs::read(root.join("guests/target/wasm32-wasip2/release/counter.wasm")).unwrap();
    let mut spec: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("guests/counter/nanovm.json")).unwrap()).unwrap();
    // A loaded host bills wall time as CPU (N-640): room to spare.
    spec["limits"]["cpu_ms"] = 200.into();
    let first = s
        .deploy(
            "Counter.localhost",
            "counter",
            wasm.clone(),
            spec.to_string().into_bytes(),
            "1.0.0",
        )
        .await
        .unwrap();

    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let send = async |method: reqwest::Method, host: &str, path: &str| {
        let r = client
            .request(method, format!("http://{}{path}", s.proxy))
            .header("host", host)
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.text().await.unwrap())
    };
    let (status, body) = send(reqwest::Method::POST, "counter.localhost", "/alice/incr").await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = send(reqwest::Method::GET, "counter.localhost:8443", "/alice/").await;
    assert_eq!(status, 200, "{body}");
    let got: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (got["object"].as_str(), got["count"].as_u64()),
        (Some("alice"), Some(1)),
        "{body}"
    );
    // The app is the host's: named by the record's owner and repository.
    assert_eq!(
        got["app"],
        nanovm::registry::app_name("counter.localhost", &first.parse().unwrap()),
        "{body}"
    );
    assert_eq!(send(reqwest::Method::GET, "nowhere.localhost", "/alice/").await.0, 404);

    // A deploy moves the tag: the next request runs it, on the same volume.
    spec["config"]["greeting"] = "redeployed".into();
    s.deploy(
        "counter.localhost",
        "counter",
        wasm,
        spec.to_string().into_bytes(),
        "1.0.1",
    )
    .await
    .unwrap();
    assert_eq!(
        send(reqwest::Method::GET, "counter.localhost", "/alice/config/greeting").await,
        (200, "redeployed".into())
    );
    let (_, body) = send(reqwest::Method::GET, "counter.localhost", "/alice/").await;
    assert!(body.contains("\"count\":1"), "{body}");
    s.stop().await;
}
