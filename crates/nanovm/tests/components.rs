//! End-to-end tests of the Wasm components in `components/` (queue, ...),
//! each composed into a small test app and run on a real in-process node.
//! Build the fixtures first with `bash scripts/build-components.sh`; the tests
//! are ignored by default for that reason and run in CI with `--ignored`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use nanovm::config::{AppConfig, NodeConfig};
use nanovm::table::Node;
use serde_json::{Value, json};

#[path = "../support/load.rs"]
#[allow(dead_code)]
mod load;
use load::request;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// The composed test app of `component`.
fn fixture(component: &str) -> Vec<u8> {
    let p = repo().join("target/components").join(component).join("tests.wasm");
    std::fs::read(&p).unwrap_or_else(|_| panic!("{} is missing: run components/{component}/build.sh", p.display()))
}

/// Limits generous enough for a loaded CI host (the CPU budget counts wall
/// time inside guest polls), with the given overrides.
fn app(component: &str, overrides: Value) -> AppConfig {
    let mut limits = json!({
        "memory_mb": 64, "cpu_ms": 5000, "request_timeout_ms": 30000,
        "disk_mb": 16, "max_files": 200,
    });
    for (k, v) in overrides.as_object().unwrap() {
        limits[k] = v.clone();
    }
    AppConfig {
        name: component.into(),
        digest: "test".into(),
        env: Default::default(),
        config: Default::default(),
        limits: serde_json::from_value(limits).unwrap(),
        worker: Default::default(),
    }
}

fn node_cfg(test: &str, max_live: usize) -> NodeConfig {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let dir = root.join("components").join(test);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cache = root.join("cwasm-cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::os::unix::fs::symlink(&cache, dir.join("cache")).unwrap();
    let cfg = NodeConfig {
        data_dir: dir,
        max_live_objects: max_live,
        memory_reservation_mb: 64,
        dev: true,
        ..NodeConfig::default()
    };
    cfg.validate().unwrap();
    cfg
}

fn start(cfg: &NodeConfig, app: &AppConfig) -> Arc<Node> {
    nanovm::engine::preflight(cfg).unwrap();
    Node::new(cfg.clone(), app, &fixture(&app.name), None).unwrap()
}

/// A node restart: everything in memory goes, the volumes stay.
async fn restart(node: Arc<Node>, cfg: &NodeConfig, app: &AppConfig) -> Arc<Node> {
    node.shutdown().await;
    drop(node);
    start(cfg, app)
}

/// Evicts every live object of a one-slot node by visiting another object.
async fn evict(node: &Arc<Node>, path: &str) {
    let before = node.metrics.evicted_lru.load(Relaxed);
    settle(node).await;
    let r = request(node, "GET", "evictor", path, "").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(node.metrics.evicted_lru.load(Relaxed) > before, "nothing was evicted");
    settle(node).await;
}

async fn settle(node: &Arc<Node>) {
    let t0 = Instant::now();
    while !node.phases().keys().all(|p| *p == "idle") {
        assert!(t0.elapsed() < Duration::from_secs(10), "objects never went idle");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// `method path` on `id`, which must answer 200; its JSON.
async fn ok(node: &Arc<Node>, method: &str, id: &str, path: &str, body: &str) -> Value {
    let r = request(node, method, id, path, body).await;
    assert_eq!(r.status, 200, "{method} {path}: {} {:?}", r.body, r.error);
    r.json()
}

fn payloads(v: &Value) -> Vec<String> {
    v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["payload"].as_str().unwrap().to_string())
        .collect()
}

fn volume(cfg: &NodeConfig, app: &str, id: &str) -> PathBuf {
    cfg.data_dir.join("objects").join(app).join(id)
}

// ---- queue ----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs components/queue/build.sh"]
async fn queue_contracts() {
    let cfg = node_cfg("queue_contracts", 4);
    let node = start(&cfg, &app("queue", json!({})));
    assert_eq!(ok(&node, "GET", "q", "/contracts", "").await, json!("ok"));
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs components/queue/build.sh"]
async fn queue_consumers_persist_across_eviction_and_restart() {
    let cfg = node_cfg("queue_persist", 1);
    let app = app("queue", json!({}));
    let mut node = start(&cfg, &app);
    assert_eq!(ok(&node, "POST", "q", "/append", "a\nb\nc").await["first"], 0);
    assert_eq!(ok(&node, "POST", "q", "/append", "d").await["first"], 3);
    let got = ok(&node, "POST", "q", "/next?max=2", "").await;
    assert_eq!(payloads(&got), ["a", "b"]);
    assert_eq!(got["messages"][1]["offset"], 1);
    // Two consumers move independently.
    assert_eq!(
        payloads(&ok(&node, "POST", "q", "/next?consumer=audit&max=10", "").await),
        ["a", "b", "c", "d"]
    );
    evict(&node, "/bounds").await;
    assert_eq!(payloads(&ok(&node, "POST", "q", "/next?max=10", "").await), ["c", "d"]);
    assert_eq!(ok(&node, "POST", "q", "/append", "e").await["first"], 4);
    node = restart(node, &cfg, &app).await;
    assert_eq!(
        ok(&node, "GET", "q", "/bounds", "").await,
        json!({"oldest": 0, "next": 5})
    );
    assert_eq!(payloads(&ok(&node, "POST", "q", "/next", "").await), ["e"]);
    assert_eq!(
        ok(&node, "GET", "q", "/consumers", "").await["consumers"],
        json!([["audit", 4], ["worker", 5]])
    );
    // Seeking back replays.
    ok(&node, "POST", "q", "/seek?offset=3", "").await;
    assert_eq!(payloads(&ok(&node, "POST", "q", "/next", "").await), ["d", "e"]);
    assert_eq!(
        payloads(&ok(&node, "GET", "q", "/read?offset=1&max=2", "").await),
        ["b", "c"]
    );
    // Objects don't share topics.
    settle(&node).await;
    assert_eq!(
        ok(&node, "GET", "other", "/bounds", "").await,
        json!({"oldest": 0, "next": 0})
    );
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs components/queue/build.sh"]
async fn queue_failed_request_rolls_back_appends_and_positions() {
    let cfg = node_cfg("queue_rollback", 2);
    let node = start(&cfg, &app("queue", json!({})));
    ok(&node, "POST", "q", "/append", "one\ntwo").await;
    // Processing failed: the position is not advanced, so they come again.
    let r = request(&node, "POST", "q", "/next?fail=1", "").await;
    assert_eq!(r.error.as_deref(), Some("guest-error"), "{}", r.body);
    assert_eq!(ok(&node, "GET", "q", "/position", "").await["position"], 0);
    let r = request(&node, "GET", "q", "/trap", "").await;
    assert_eq!(r.error.as_deref(), Some("trap"), "{}", r.body);
    assert_eq!(
        ok(&node, "GET", "q", "/bounds", "").await,
        json!({"oldest": 0, "next": 2})
    );
    assert_eq!(payloads(&ok(&node, "POST", "q", "/next", "").await), ["one", "two"]);
    assert_eq!(ok(&node, "POST", "q", "/append", "three").await["first"], 2);
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs components/queue/build.sh"]
async fn queue_segments_roll_and_trim() {
    let cfg = node_cfg("queue_trim", 2);
    let node = start(&cfg, &app("queue", json!({})));
    // 40,000 eight-byte messages: three segments of at most 16,384.
    ok(&node, "POST", "q", "/bulk?count=40000&size=8&batch=1024", "").await;
    let dir = volume(&cfg, "queue", "q").join(".queue/jobs");
    let segments = |dir: &Path| {
        let mut s: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| {
                e.unwrap()
                    .file_name()
                    .into_string()
                    .unwrap()
                    .strip_suffix(".idx")
                    .map(String::from)
            })
            .collect();
        s.sort();
        s
    };
    assert_eq!(
        segments(&dir),
        ["00000000000000000000", "00000000000000016384", "00000000000000032768"]
    );
    // A read across a segment boundary.
    let got = ok(&node, "GET", "q", "/read?offset=16382&max=4", "").await;
    assert_eq!(payloads(&got), ["00016382", "00016383", "00016384", "00016385"]);
    ok(&node, "POST", "q", "/seek?offset=100", "").await;
    ok(&node, "POST", "q", "/trim?before=20000", "").await;
    assert_eq!(segments(&dir), ["00000000000000016384", "00000000000000032768"]);
    assert_eq!(
        ok(&node, "GET", "q", "/bounds", "").await,
        json!({"oldest": 20000, "next": 40000})
    );
    // A consumer behind the trim point moves up to the oldest message.
    assert_eq!(ok(&node, "GET", "q", "/position", "").await["position"], 20000);
    assert_eq!(
        payloads(&ok(&node, "GET", "q", "/read?offset=5&max=1", "").await),
        ["00020000"]
    );
    let r = request(&node, "POST", "q", "/trim?before=40001", "").await;
    assert_eq!(r.status, 409, "{}", r.body);
    // Trimming everything keeps offsets increasing.
    ok(&node, "POST", "q", "/trim?before=40000", "").await;
    assert!(segments(&dir).is_empty());
    assert_eq!(ok(&node, "POST", "q", "/append", "after").await["first"], 40000);
    assert_eq!(
        ok(&node, "GET", "q", "/bounds", "").await,
        json!({"oldest": 40000, "next": 40001})
    );
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs components/queue/build.sh"]
async fn queue_quota_exhaustion_is_refused_cleanly() {
    let cfg = node_cfg("queue_quota", 2);
    let app = app("queue", json!({"disk_mb": 1}));
    let mut node = start(&cfg, &app);
    // 64 KiB messages until the 1 MiB quota refuses one; the probe checks
    // the refused append changed nothing, and the request commits the rest.
    let full = ok(&node, "POST", "q", "/fill", "").await;
    assert_eq!(full["error"], "Error::Full");
    let appended = full["appended"].as_u64().unwrap();
    assert!((10..16).contains(&appended), "{full}");
    node = restart(node, &cfg, &app).await;
    assert_eq!(ok(&node, "GET", "q", "/bounds", "").await["next"], appended);
    // Trimming frees the space at commit; appends work again.
    ok(&node, "POST", "q", &format!("/trim?before={appended}"), "").await;
    assert_eq!(ok(&node, "POST", "q", "/append", "fits").await["first"], appended);
    node.shutdown().await;
    // Out of directory entries: a new topic is refused whole, and an existing
    // consumer moves on while a new one is refused.
    let cfg = node_cfg("queue_files", 2);
    let app = crate::app("queue", json!({"max_files": 4}));
    let node = start(&cfg, &app);
    ok(&node, "POST", "q", "/append", "x").await; // .queue, jobs, .idx, .dat
    let r = request(&node, "POST", "q", "/append?topic=other", "y").await;
    assert_eq!(
        (r.status, r.json()["error"].as_str()),
        (409, Some("Error::Full")),
        "{}",
        r.body
    );
    let r = request(&node, "POST", "q", "/seek?offset=1", "").await;
    assert_eq!(
        (r.status, r.json()["error"].as_str()),
        (409, Some("Error::Full")),
        "{}",
        r.body
    );
    assert_eq!(ok(&node, "GET", "q", "/consumers", "").await["consumers"], json!([]));
    assert_eq!(
        ok(&node, "GET", "q", "/bounds?topic=other", "").await,
        json!({"oldest": 0, "next": 0})
    );
    let entries: Vec<_> = std::fs::read_dir(volume(&cfg, "queue", "q").join(".queue"))
        .unwrap()
        .collect();
    assert_eq!(entries.len(), 1, "a refused topic left a directory");
    node.shutdown().await;
}

// ---- docs -----------------------------------------------------------------

fn ids(v: &Value) -> Vec<String> {
    v["ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs components/docs/build.sh"]
async fn docs_contracts() {
    let cfg = node_cfg("docs_contracts", 4);
    let node = start(&cfg, &app("docs", json!({})));
    assert_eq!(ok(&node, "GET", "d", "/contracts", "").await, json!("ok"));
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs components/docs/build.sh"]
async fn docs_indexes_survive_eviction_and_restart() {
    let cfg = node_cfg("docs_persist", 1);
    let app = app("docs", json!({}));
    let mut node = start(&cfg, &app);
    ok(&node, "POST", "d", "/index?field=email&unique=1", "").await;
    for (id, body) in [
        ("ann", r#"{"email":"ann@x","city":"Oslo","age":31}"#),
        ("bob", r#"{"email":"bob@x","city":"Lima","age":25}"#),
        ("cat", r#"{"email":"cat@x","city":"Oslo","age":40}"#),
    ] {
        ok(&node, "PUT", "d", &format!("/doc?id={id}"), body).await;
    }
    // An index created later covers existing documents.
    ok(&node, "POST", "d", "/index?field=city", "").await;
    ok(&node, "POST", "d", "/index?field=age", "").await;
    assert_eq!(
        ids(&ok(
            &node,
            "GET",
            "d",
            &format!("/query?field=city&eq={}", urlencode("\"Oslo\"")),
            ""
        )
        .await),
        ["ann", "cat"]
    );
    // A unique index refuses a second owner of a value, and changes nothing.
    let r = request(&node, "PUT", "d", "/doc?id=dan", r#"{"email":"ann@x","city":"Rome"}"#).await;
    assert_eq!(
        (r.status, r.json()["error"].as_str()),
        (409, Some("Error::Conflict(\"email\")")),
        "{}",
        r.body
    );
    assert_eq!(request(&node, "GET", "d", "/doc?id=dan", "").await.status, 404);
    let r = request(&node, "POST", "d", "/index?field=city&unique=1", "").await;
    assert_eq!(r.status, 409, "{}", r.body);
    // Replacing a document moves it in every index.
    ok(
        &node,
        "PUT",
        "d",
        "/doc?id=ann",
        r#"{"email":"ann@y","city":"Lima","age":32}"#,
    )
    .await;
    evict(&node, "/stats").await;
    assert_eq!(
        ids(&ok(
            &node,
            "GET",
            "d",
            &format!("/query?field=city&eq={}", urlencode("\"Lima\"")),
            ""
        )
        .await),
        ["ann", "bob"]
    );
    assert_eq!(
        ids(&ok(
            &node,
            "GET",
            "d",
            &format!("/query?field=email&eq={}", urlencode("\"ann@x\"")),
            ""
        )
        .await),
        Vec::<String>::new()
    );
    ok(
        &node,
        "PUT",
        "d",
        "/doc?id=dan",
        r#"{"email":"ann@x","city":"Rome","age":25}"#,
    )
    .await;
    assert_eq!(ok(&node, "DELETE", "d", "/doc?id=bob", "").await["existed"], true);
    node = restart(node, &cfg, &app).await;
    assert_eq!(
        ids(&ok(&node, "GET", "d", "/query?field=age&gte=25&lt=40", "").await),
        ["dan", "ann"]
    );
    let page = ok(&node, "GET", "d", "/query?field=age&limit=1", "").await;
    assert_eq!(ids(&page), ["dan"]);
    let cursor = page["cursor"].as_str().unwrap();
    let next = ok(
        &node,
        "GET",
        "d",
        &format!("/query?field=age&limit=5&after={}", urlencode(cursor)),
        "",
    )
    .await;
    assert_eq!(
        (ids(&next), &next["cursor"]),
        (vec!["ann".to_string(), "cat".to_string()], &Value::Null)
    );
    assert_eq!(ok(&node, "GET", "d", "/doc?id=ann", "").await["email"], "ann@y");
    assert_eq!(
        ok(&node, "GET", "d", "/indexes", "").await,
        json!([{"field": "email", "unique": true}, {"field": "city", "unique": false}, {"field": "age", "unique": false}])
    );
    let listed: Vec<_> = ok(&node, "GET", "d", "/list", "")
        .await
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].clone())
        .collect();
    assert_eq!(listed, ["ann", "cat", "dan"]);
    node.shutdown().await;
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
            b => format!("%{b:02X}"),
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs components/docs/build.sh"]
async fn docs_failed_request_rolls_back() {
    let cfg = node_cfg("docs_rollback", 2);
    let node = start(&cfg, &app("docs", json!({})));
    ok(&node, "PUT", "d", "/doc?id=a", r#"{"v":1}"#).await;
    // A failed request's put and new index both roll back, memory included.
    let r = request(&node, "POST", "d", "/fail?id=a&field=v", r#"{"v":2}"#).await;
    assert_eq!(r.error.as_deref(), Some("guest-error"), "{}", r.body);
    assert_eq!(ok(&node, "GET", "d", "/doc?id=a", "").await, json!({"v": 1}));
    assert_eq!(ok(&node, "GET", "d", "/indexes", "").await, json!([]));
    let r = request(&node, "POST", "d", "/trap?id=a", "").await;
    assert_eq!(r.error.as_deref(), Some("trap"), "{}", r.body);
    assert_eq!(ok(&node, "GET", "d", "/doc?id=a", "").await, json!({"v": 1}));
    assert_eq!(ok(&node, "GET", "d", "/stats", "").await["documents"], 1);
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs components/docs/build.sh"]
async fn docs_log_is_compacted() {
    let cfg = node_cfg("docs_compact", 2);
    let app = app("docs", json!({}));
    let mut node = start(&cfg, &app);
    ok(&node, "POST", "d", "/index?field=tag", "").await;
    // 100 documents of ~4 KiB, rewritten three times: compaction keeps the
    // log under twice its live bytes once it passes 1 MiB.
    for _ in 0..3 {
        ok(&node, "POST", "d", "/bulk?count=100&size=4000", "").await;
    }
    let stats = ok(&node, "GET", "d", "/stats", "").await;
    let (live, log) = (stats["live"].as_u64().unwrap(), stats["log"].as_u64().unwrap());
    assert_eq!(stats["documents"], 100);
    assert!(log > live && log <= 2 * live + 5000, "{stats}");
    let file = volume(&cfg, "docs", "d").join(".docs/people/log");
    assert_eq!(std::fs::metadata(&file).unwrap().len(), log);
    ok(&node, "POST", "d", "/compact", "").await;
    assert_eq!(std::fs::metadata(&file).unwrap().len(), live);
    node = restart(node, &cfg, &app).await;
    assert_eq!(
        ok(&node, "GET", "d", "/stats", "").await,
        json!({"documents": 100, "live": live, "log": live})
    );
    assert_eq!(
        ids(&ok(
            &node,
            "GET",
            "d",
            &format!("/query?field=tag&eq={}&limit=3", urlencode("\"t3\"")),
            ""
        )
        .await),
        ["d000003", "d000013", "d000023"]
    );
    assert_eq!(ok(&node, "GET", "d", "/doc?id=d000099", "").await["n"], 99);
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs components/docs/build.sh"]
async fn docs_quota_exhaustion_is_refused_cleanly() {
    let cfg = node_cfg("docs_quota", 2);
    let app = app("docs", json!({"disk_mb": 1}));
    let mut node = start(&cfg, &app);
    let long = "L".repeat(200);
    ok(&node, "PUT", "d", &format!("/doc?id={long}"), r#"{"long":true}"#).await;
    let full = ok(&node, "POST", "d", "/fill?fine=1", "").await;
    assert_eq!(full["error"], "Error::Full");
    let stored = full["stored"].as_u64().unwrap();
    assert!(stored > 15, "{full}");
    node = restart(node, &cfg, &app).await;
    assert_eq!(ok(&node, "GET", "d", "/stats", "").await["documents"], stored + 1);
    // Not even its delete record fits now: the delete marks its record dead in place.
    assert_eq!(
        ok(&node, "DELETE", "d", &format!("/doc?id={long}"), "").await["existed"],
        true
    );
    assert_eq!(
        request(&node, "GET", "d", &format!("/doc?id={long}"), "").await.status,
        404
    );
    node = restart(node, &cfg, &app).await;
    assert_eq!(
        request(&node, "GET", "d", &format!("/doc?id={long}"), "").await.status,
        404
    );
    assert_eq!(ok(&node, "GET", "d", "/stats", "").await["documents"], stored);
    // A full volume still lets documents go (the log is rewritten in place
    // when a delete record doesn't fit), and compaction frees the space
    // without needing room for a second copy.
    let r = request(
        &node,
        "PUT",
        "d",
        "/doc?id=more",
        &format!(r#"{{"pad":"{}"}}"#, "z".repeat(100_000)),
    )
    .await;
    assert_eq!(
        (r.status, r.json()["error"].as_str()),
        (409, Some("Error::Full")),
        "{}",
        r.body
    );
    for n in 0..stored - 1 {
        assert_eq!(
            ok(&node, "DELETE", "d", &format!("/doc?id=fill{n}"), "").await["existed"],
            true
        );
    }
    assert_eq!(ok(&node, "GET", "d", "/stats", "").await["documents"], 1);
    ok(&node, "POST", "d", "/compact", "").await;
    let stats = ok(&node, "GET", "d", "/stats", "").await;
    assert_eq!(
        (stats["documents"].as_u64(), stats["log"].as_u64()),
        (Some(1), stats["live"].as_u64())
    );
    ok(
        &node,
        "PUT",
        "d",
        "/doc?id=more",
        &format!(r#"{{"pad":"{}"}}"#, "z".repeat(500_000)),
    )
    .await;
    node = restart(node, &cfg, &app).await;
    assert_eq!(ok(&node, "GET", "d", "/stats", "").await["documents"], 2);
    assert_eq!(
        ok(&node, "GET", "d", &format!("/doc?id=fill{}", stored - 1), "").await["n"],
        stored - 1
    );
    node.shutdown().await;
}

// ---- timeseries -----------------------------------------------------------

const HOUR: u64 = 3_600_000;
const DAY: u64 = 24 * HOUR;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs components/timeseries/build.sh"]
async fn timeseries_contracts() {
    let cfg = node_cfg("timeseries_contracts", 4);
    let node = start(&cfg, &app("timeseries", json!({})));
    assert_eq!(ok(&node, "GET", "t", "/contracts", "").await, json!("ok"));
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs components/timeseries/build.sh"]
async fn timeseries_rollups_survive_eviction_restart_and_raw_trim() {
    let cfg = node_cfg("timeseries_persist", 1);
    let app = app("timeseries", json!({}));
    let mut node = start(&cfg, &app);
    // Three days of one point a minute (value = minute index), in two
    // requests, the second out of order.
    let day0 = 20_000 * DAY;
    ok(&node, "POST", "t", &format!("/write?n=2880&start={day0}&every=60000"), "").await;
    ok(&node, "POST", "t", &format!("/write?n=1440&start={}&every=60000", day0 + 2 * DAY), "").await;
    let days = ok(&node, "GET", "t", &format!("/agg?from={day0}&to={}&step={DAY}", day0 + 3 * DAY), "").await;
    let sum = |a: u64, b: u64| ((a..b).sum::<u64>()) as f64;
    assert_eq!(
        days,
        json!([
            [day0, 1440, sum(0, 1440), 0.0, 1439.0],
            [day0 + DAY, 1440, sum(1440, 2880), 1440.0, 2879.0],
            [day0 + 2 * DAY, 1440, sum(0, 1440), 0.0, 1439.0]
        ])
    );
    evict(&node, "/latest").await;
    let hours = ok(&node, "GET", "t", &format!("/agg?from={}&to={}&step={HOUR}", day0 + DAY, day0 + DAY + 2 * HOUR), "").await;
    assert_eq!(
        hours,
        json!([[day0 + DAY, 60, sum(1440, 1500), 1440.0, 1499.0], [day0 + DAY + HOUR, 60, sum(1500, 1560), 1500.0, 1559.0]])
    );
    // A 15-minute step reads raw points.
    let quarter = ok(&node, "GET", "t", &format!("/agg?from={day0}&to={}&step=900000", day0 + HOUR), "").await;
    assert_eq!(quarter.as_array().unwrap().len(), 4);
    assert_eq!(quarter[1], json!([day0 + 900_000, 15, sum(15, 30), 15.0, 29.0]));
    node = restart(node, &cfg, &app).await;
    assert_eq!(ok(&node, "GET", "t", "/latest", "").await, json!([day0 + 3 * DAY - 60_000, 1439.0]));
    let range = ok(&node, "GET", "t", &format!("/range?from={}&to={}", day0 + DAY - 60_000, day0 + DAY + 60_001), "").await;
    assert_eq!(range, json!([[day0 + DAY - 60_000, 1439.0], [day0 + DAY, 1440.0], [day0 + DAY + 60_000, 1441.0]]));
    // Retention: drop the first two days of raw points; rollups remain.
    ok(&node, "POST", "t", &format!("/trim?level=raw&before={}", day0 + 2 * DAY + 5), "").await;
    let dir = volume(&cfg, "timeseries", "t").join(".ts/cpu");
    let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    files.sort();
    assert_eq!(files, [format!("d{}", 20_000 / 1024), format!("h{}", 20_000 * 24 / 768), format!("r{}", 20_002)]);
    assert_eq!(ok(&node, "GET", "t", &format!("/range?to={}", day0 + 2 * DAY), "").await, json!([]));
    assert_eq!(ok(&node, "GET", "t", &format!("/agg?from={day0}&to={}&step={DAY}", day0 + 3 * DAY), "").await, days);
    // Objects don't share series.
    settle(&node).await;
    assert_eq!(ok(&node, "GET", "other", "/latest", "").await, Value::Null);
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs components/timeseries/build.sh"]
async fn timeseries_failed_request_rolls_back() {
    let cfg = node_cfg("timeseries_rollback", 2);
    let node = start(&cfg, &app("timeseries", json!({})));
    ok(&node, "POST", "t", "/write", "1000 1\n2000 2").await;
    for probe in ["/fail", "/trap"] {
        let r = request(&node, "POST", "t", probe, "1500 100\n90000000 5").await;
        assert!(r.error.is_some(), "{probe}: {}", r.body);
    }
    assert_eq!(ok(&node, "GET", "t", "/range", "").await, json!([[1000, 1.0], [2000, 2.0]]));
    assert_eq!(ok(&node, "GET", "t", &format!("/agg?from=0&to={}&step={DAY}", 2 * DAY), "").await, json!([[0, 2, 3.0, 1.0, 2.0]]));
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs components/timeseries/build.sh"]
async fn timeseries_quota_exhaustion_is_refused_cleanly() {
    let cfg = node_cfg("timeseries_quota", 2);
    let app = app("timeseries", json!({"disk_mb": 1}));
    let mut node = start(&cfg, &app);
    let full = ok(&node, "POST", "t", "/fill", "").await;
    assert_eq!(full["error"], "Error::Full");
    let batches = full["batches"].as_u64().unwrap();
    assert!((50..66).contains(&batches), "{full}");
    node = restart(node, &cfg, &app).await;
    // Raw points and both rollups agree: the refused batch left nothing.
    let points = batches * 1000;
    let raw = ok(&node, "GET", "t", "/agg?from=0&to=100000000&step=100000000", "").await;
    let day = ok(&node, "GET", "t", &format!("/agg?from=0&to={DAY}&step={DAY}"), "").await;
    assert_eq!(raw[0][1], points);
    assert_eq!(day[0][1], points);
    node.shutdown().await;
    // Out of directory entries: a batch spanning an existing day and a new
    // one is refused whole, including the existing day's raw points and
    // rollups.
    let cfg = node_cfg("timeseries_files", 2);
    let app = crate::app("timeseries", json!({"max_files": 5}));
    let node = start(&cfg, &app);
    ok(&node, "POST", "t", "/write", "1000 1").await; // .ts, cpu, r0, h0, d0
    let r = request(&node, "POST", "t", "/write", &format!("2000 2\n{} 3", DAY + 1)).await;
    assert_eq!((r.status, r.json()["error"].as_str()), (409, Some("Error::Full")), "{}", r.body);
    assert_eq!(ok(&node, "GET", "t", "/range", "").await, json!([[1000, 1.0]]));
    assert_eq!(ok(&node, "GET", "t", &format!("/agg?from=0&to={DAY}&step={HOUR}"), "").await, json!([[0, 1, 1.0, 1.0, 1.0]]));
    assert_eq!(ok(&node, "GET", "t", &format!("/agg?from=0&to={DAY}&step={DAY}"), "").await, json!([[0, 1, 1.0, 1.0, 1.0]]));
    let raw = volume(&cfg, "timeseries", "t").join(".ts/cpu/r0");
    assert_eq!(std::fs::metadata(raw).unwrap().len(), 16);
    node.shutdown().await;
}
