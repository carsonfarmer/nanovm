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
    ok(
        &node,
        "POST",
        "t",
        &format!("/write?n=2880&start={day0}&every=60000"),
        "",
    )
    .await;
    ok(
        &node,
        "POST",
        "t",
        &format!("/write?n=1440&start={}&every=60000", day0 + 2 * DAY),
        "",
    )
    .await;
    let days = ok(
        &node,
        "GET",
        "t",
        &format!("/agg?from={day0}&to={}&step={DAY}", day0 + 3 * DAY),
        "",
    )
    .await;
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
    let hours = ok(
        &node,
        "GET",
        "t",
        &format!("/agg?from={}&to={}&step={HOUR}", day0 + DAY, day0 + DAY + 2 * HOUR),
        "",
    )
    .await;
    assert_eq!(
        hours,
        json!([
            [day0 + DAY, 60, sum(1440, 1500), 1440.0, 1499.0],
            [day0 + DAY + HOUR, 60, sum(1500, 1560), 1500.0, 1559.0]
        ])
    );
    // A 15-minute step reads raw points.
    let quarter = ok(
        &node,
        "GET",
        "t",
        &format!("/agg?from={day0}&to={}&step=900000", day0 + HOUR),
        "",
    )
    .await;
    assert_eq!(quarter.as_array().unwrap().len(), 4);
    assert_eq!(quarter[1], json!([day0 + 900_000, 15, sum(15, 30), 15.0, 29.0]));
    node = restart(node, &cfg, &app).await;
    assert_eq!(
        ok(&node, "GET", "t", "/latest", "").await,
        json!([day0 + 3 * DAY - 60_000, 1439.0])
    );
    let range = ok(
        &node,
        "GET",
        "t",
        &format!("/range?from={}&to={}", day0 + DAY - 60_000, day0 + DAY + 60_001),
        "",
    )
    .await;
    assert_eq!(
        range,
        json!([
            [day0 + DAY - 60_000, 1439.0],
            [day0 + DAY, 1440.0],
            [day0 + DAY + 60_000, 1441.0]
        ])
    );
    // Retention: drop the first two days of raw points; rollups remain.
    ok(
        &node,
        "POST",
        "t",
        &format!("/trim?level=raw&before={}", day0 + 2 * DAY + 5),
        "",
    )
    .await;
    let dir = volume(&cfg, "timeseries", "t").join(".ts/cpu");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    files.sort();
    assert_eq!(
        files,
        [
            format!("d{}", 20_000 / 1024),
            format!("h{}", 20_000 * 24 / 768),
            format!("r{}", 20_002)
        ]
    );
    assert_eq!(
        ok(&node, "GET", "t", &format!("/range?to={}", day0 + 2 * DAY), "").await,
        json!([])
    );
    assert_eq!(
        ok(
            &node,
            "GET",
            "t",
            &format!("/agg?from={day0}&to={}&step={DAY}", day0 + 3 * DAY),
            ""
        )
        .await,
        days
    );
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
    assert_eq!(
        ok(&node, "GET", "t", "/range", "").await,
        json!([[1000, 1.0], [2000, 2.0]])
    );
    assert_eq!(
        ok(&node, "GET", "t", &format!("/agg?from=0&to={}&step={DAY}", 2 * DAY), "").await,
        json!([[0, 2, 3.0, 1.0, 2.0]])
    );
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
    assert_eq!(
        (r.status, r.json()["error"].as_str()),
        (409, Some("Error::Full")),
        "{}",
        r.body
    );
    assert_eq!(ok(&node, "GET", "t", "/range", "").await, json!([[1000, 1.0]]));
    assert_eq!(
        ok(&node, "GET", "t", &format!("/agg?from=0&to={DAY}&step={HOUR}"), "").await,
        json!([[0, 1, 1.0, 1.0, 1.0]])
    );
    assert_eq!(
        ok(&node, "GET", "t", &format!("/agg?from=0&to={DAY}&step={DAY}"), "").await,
        json!([[0, 1, 1.0, 1.0, 1.0]])
    );
    let raw = volume(&cfg, "timeseries", "t").join(".ts/cpu/r0");
    assert_eq!(std::fs::metadata(raw).unwrap().len(), 16);
    node.shutdown().await;
}

// ---- templates ------------------------------------------------------------
// Pure computation: no volume, so nothing to persist, roll back or fill.

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs components/templates/build.sh"]
async fn templates_contracts() {
    let cfg = node_cfg("templates_contracts", 4);
    let node = start(&cfg, &app("templates", json!({})));
    assert_eq!(ok(&node, "GET", "t", "/contracts", "").await, json!("ok"));
    let r = request(&node, "GET", "t", "/page?via=component&n=3", "").await;
    assert_eq!(r.status, 200);
    assert!(
        r.body
            .starts_with("<!doctype html><title>Ada &amp; co's orders</title>"),
        "{}",
        r.body
    );
    assert_eq!(r.body, request(&node, "GET", "t", "/page?via=crate", "").await.body);
    node.shutdown().await;
}

// ---- auth -----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs components/auth/build.sh"]
async fn auth_jwt_and_password_contracts() {
    let cfg = node_cfg("auth_contracts", 4);
    let node = start(&cfg, &app("auth", json!({})));
    assert_eq!(ok(&node, "GET", "a", "/jwt-contracts", "").await, json!("ok"));
    assert_eq!(ok(&node, "GET", "a", "/password-contracts", "").await, json!("ok"));
    assert_eq!(ok(&node, "GET", "a", "/session/contracts", "").await, json!("ok"));
    // The default (OWASP minimum) cost fits a request's CPU budget.
    let hash = ok(&node, "POST", "a", "/password", "hunter2").await["hash"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(hash.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"), "{hash}");
    let path = format!("/password?hash={}", urlencode(&hash));
    assert_eq!(ok(&node, "POST", "a", &path, "hunter2").await, json!({"ok": true}));
    assert_eq!(ok(&node, "POST", "a", &path, "hunter3").await, json!({"ok": false}));
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs components/auth/build.sh"]
async fn auth_jwks_url_is_fetched_once_and_cached_in_the_volume() {
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    let vectors: Value =
        serde_json::from_str(&std::fs::read_to_string(repo().join("components/auth/tests/src/vectors.json")).unwrap())
            .unwrap();
    let full = vectors["jwks"].as_str().unwrap().to_string();
    // Without the Ed25519 key, as before a rotation.
    let mut set: Value = serde_json::from_str(&full).unwrap();
    set["keys"].as_array_mut().unwrap().retain(|k| k["kid"] != "ed1");
    let before = set.to_string();
    let (hits, rotated, broken) = (
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (h, r, b) = (hits.clone(), rotated.clone(), broken.clone());
    let server = tokio::spawn(nanovm::server::serve_http1(listener, move |_req| {
        h.fetch_add(1, Relaxed);
        let body = if r.load(Relaxed) { full.clone() } else { before.clone() };
        let broken = b.load(Relaxed);
        async move {
            match broken {
                true => nanovm::object::error_response(500, "down", "down"),
                false => nanovm::server::ok_json(serde_json::from_str(&body).unwrap()),
            }
        }
    }));
    let cfg = node_cfg("auth_jwks", 1);
    let app = app("auth", json!({"egress": [format!("127.0.0.1:{port}")]}));
    let mut node = start(&cfg, &app);
    let url = urlencode(&format!("http://127.0.0.1:{port}/jwks"));
    let verify = |case: &str, ttl: u32| format!("/jwks-url?case={case}&url={url}&ttl={ttl}");
    assert_eq!(ok(&node, "GET", "a", &verify("rs256", 3600), "").await["sub"], "alice");
    assert_eq!(ok(&node, "GET", "a", &verify("es256", 3600), "").await["sub"], "alice");
    assert_eq!(hits.load(Relaxed), 1, "fetched once");
    // Within a minute of the fetch, an unknown kid doesn't refetch.
    let r = request(&node, "GET", "a", &verify("eddsa", 3600), "").await;
    assert!(r.body.contains("NoKey"), "{}", r.body);
    assert_eq!(hits.load(Relaxed), 1);
    // A restart reads the cached set from the volume.
    node = restart(node, &cfg, &app).await;
    assert_eq!(ok(&node, "GET", "a", &verify("rs256", 3600), "").await["sub"], "alice");
    assert_eq!(hits.load(Relaxed), 1);
    // The issuer rotates in a new key; once the cache is a minute old, the
    // new kid refetches (aged here by editing the cached file while stopped).
    node.shutdown().await;
    drop(node);
    let dir = volume(&cfg, "auth", "a").join(".auth/jwks");
    let file = std::fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
    let mut cached: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    cached["fetched"] = json!(cached["fetched"].as_u64().unwrap() - 120);
    std::fs::write(&file, cached.to_string()).unwrap();
    rotated.store(true, Relaxed);
    node = start(&cfg, &app);
    assert_eq!(ok(&node, "GET", "a", &verify("eddsa", 3600), "").await["sub"], "alice");
    assert_eq!(hits.load(Relaxed), 2);
    // An expired cache refetches; if the issuer is down, the stale set serves.
    broken.store(true, Relaxed);
    assert_eq!(ok(&node, "GET", "a", &verify("es256", 0), "").await["sub"], "alice");
    assert_eq!(hits.load(Relaxed), 3);
    // With no cache at all, the failure is reported.
    let other = urlencode(&format!("http://127.0.0.1:{port}/other"));
    let r = request(&node, "GET", "a", &format!("/jwks-url?case=rs256&url={other}"), "").await;
    assert!(
        r.status == 409 && r.body.contains("Fetch") && r.body.contains("500"),
        "{}",
        r.body
    );
    node.shutdown().await;
    server.abort();
}

/// Every file under `dir`, recursively.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten() {
            let p = e.unwrap().path();
            if p.is_dir() { stack.push(p) } else { out.push(p) }
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs components/auth/build.sh"]
async fn auth_sessions_persist_expire_and_roll_back() {
    let cfg = node_cfg("auth_sessions", 1);
    let app = app("auth", json!({}));
    let mut node = start(&cfg, &app);
    let create = |subject: &str, ttl: u32| format!("/session/create?subject={subject}&ttl={ttl}");
    let token = ok(&node, "POST", "s", &create("alice", 3600), "{\"cart\":3}").await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let get = format!("/session/get?token={token}");
    assert_eq!(
        ok(&node, "GET", "s", &get, "").await,
        json!({"subject": "alice", "data": "{\"cart\":3}", "ttl": 3600})
    );
    // The volume never holds the token itself.
    let sessions = volume(&cfg, "auth", "s").join(".auth/sessions");
    for f in files_under(&sessions) {
        assert!(!String::from_utf8_lossy(&std::fs::read(f).unwrap()).contains(&token));
    }
    evict(&node, "/session/get?token=x").await;
    assert_eq!(ok(&node, "GET", "s", &get, "").await["subject"], "alice");
    node = restart(node, &cfg, &app).await;
    assert_eq!(ok(&node, "GET", "s", &get, "").await["subject"], "alice");
    // A failed request's session never existed.
    let count = || files_under(&sessions).len();
    let r = request(&node, "POST", "s", "/session/fail", "").await;
    assert_eq!(r.error.as_deref(), Some("guest-error"), "{}", r.body);
    // The next instance start replays the undo journal.
    assert_eq!(ok(&node, "GET", "s", &get, "").await["subject"], "alice");
    assert_eq!(count(), 1);
    // Log out everywhere ends only that subject's sessions.
    for _ in 0..3 {
        ok(&node, "POST", "s", &create("bob", 3600), "").await;
    }
    assert_eq!(
        ok(&node, "POST", "s", "/session/destroy-subject?subject=bob", "").await,
        3
    );
    assert_eq!(count(), 1);
    assert_eq!(ok(&node, "GET", "s", &get, "").await["subject"], "alice");
    // Expiry, refresh and sweeping.
    ok(&node, "POST", "s", &format!("/session/refresh?token={token}&ttl=1"), "").await;
    let short = ok(&node, "POST", "s", &create("dave", 1), "").await["token"]
        .as_str()
        .unwrap()
        .to_string();
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert_eq!(ok(&node, "GET", "s", &get, "").await, Value::Null);
    assert_eq!(
        ok(&node, "POST", "s", &format!("/session/refresh?token={short}"), "").await,
        false
    );
    assert_eq!(ok(&node, "POST", "s", "/session/sweep", "").await, 2);
    assert_eq!(count(), 0);
    let live = ok(&node, "POST", "s", &create("erin", 3600), "").await["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        ok(&node, "POST", "s", &format!("/session/destroy?token={live}"), "").await,
        true
    );
    assert_eq!(
        ok(&node, "POST", "s", &format!("/session/destroy?token={live}"), "").await,
        false
    );
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs components/auth/build.sh"]
async fn auth_sessions_quota_exhaustion_is_refused_cleanly() {
    let cfg = node_cfg("auth_quota", 2);
    // .auth, .auth/sessions and three session files.
    let app = app("auth", json!({"max_files": 5}));
    let node = start(&cfg, &app);
    let mut tokens = Vec::new();
    for _ in 0..3 {
        tokens.push(
            ok(&node, "POST", "s", "/session/create", "").await["token"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }
    let r = request(&node, "POST", "s", "/session/create", "").await;
    assert_eq!(
        (r.status, r.json()["error"].as_str()),
        (409, Some("Error::Full")),
        "{}",
        r.body
    );
    assert_eq!(files_under(&volume(&cfg, "auth", "s")).len(), 3);
    assert_eq!(
        ok(&node, "POST", "s", &format!("/session/destroy?token={}", tokens[0]), "").await,
        true
    );
    ok(&node, "POST", "s", "/session/create", "").await;
    // A refresh needs a temporary entry: refused whole on a full volume.
    let r = request(
        &node,
        "POST",
        "s",
        &format!("/session/refresh?token={}&ttl=60", tokens[1]),
        "",
    )
    .await;
    assert_eq!(
        (r.status, r.json()["error"].as_str()),
        (409, Some("Error::Full")),
        "{}",
        r.body
    );
    let session = ok(&node, "GET", "s", &format!("/session/get?token={}", tokens[1]), "").await;
    assert_eq!(session["ttl"], 3600);
    assert_eq!(files_under(&volume(&cfg, "auth", "s")).len(), 3);
    node.shutdown().await;
    // Out of bytes: a session whose write is refused leaves no file.
    let cfg = node_cfg("auth_bytes", 2);
    let node = start(&cfg, &crate::app("auth", json!({"disk_mb": 1})));
    let data = "d".repeat(60 << 10);
    let mut created = 0;
    loop {
        let r = request(&node, "POST", "s", "/session/create", &data).await;
        if r.status == 409 {
            assert_eq!(r.json()["error"], "Error::Full", "{}", r.body);
            break;
        }
        assert_eq!(r.status, 200, "{}", r.body);
        created += 1;
    }
    assert!((10..18).contains(&created), "{created}");
    assert_eq!(files_under(&volume(&cfg, "auth", "s")).len(), created);
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs components/docs/build.sh"]
async fn docs_full_text_search() {
    let cfg = node_cfg("docs_search", 2);
    let app = app("docs", json!({}));
    let mut node = start(&cfg, &app);
    ok(&node, "POST", "d", "/text-index?field=body", "").await;
    for (id, body) in [
        ("a", r#"{"body":"The quick brown fox jumps over the lazy dog"}"#),
        ("b", r#"{"body":"A quick, QUICK start guide to Rust"}"#),
        ("c", r#"{"body":["Rust components","over the filesystem"]}"#),
        ("d", r#"{"body":42}"#),
    ] {
        ok(&node, "PUT", "d", &format!("/doc?id={id}"), body).await;
    }
    let search = |q: &str| format!("/search?field=body&q={}", urlencode(q));
    let ranked = |v: Value| {
        v.as_array()
            .unwrap()
            .iter()
            .map(|h| h[0].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    // Twice the term in a shorter document ranks first; case doesn't matter.
    let quick = ok(&node, "GET", "d", &search("Quick"), "").await;
    assert_eq!(ranked(quick.clone()), ["b", "a"]);
    assert!(quick[0][1].as_f64().unwrap() > quick[1][1].as_f64().unwrap());
    assert_eq!(
        ranked(ok(&node, "GET", "d", &search("rust filesystem"), "").await),
        ["c", "b"]
    );
    assert_eq!(ranked(ok(&node, "GET", "d", &search("over"), "").await), ["c", "a"]);
    assert_eq!(ok(&node, "GET", "d", &search("nothing here"), "").await, json!([]));
    assert_eq!(
        ok(&node, "GET", "d", &format!("{}&limit=1", search("quick rust")), "")
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // Replacing and deleting documents update the index.
    ok(
        &node,
        "PUT",
        "d",
        "/doc?id=b",
        r#"{"body":"A slow start guide to Rust"}"#,
    )
    .await;
    assert_eq!(ranked(ok(&node, "GET", "d", &search("quick"), "").await), ["a"]);
    ok(&node, "DELETE", "d", "/doc?id=a", "").await;
    assert_eq!(ok(&node, "GET", "d", &search("quick"), "").await, json!([]));
    // Text indexes are searched, not ranged; other indexes are ranged, not searched.
    let r = request(&node, "GET", "d", "/query?field=body", "").await;
    assert!(r.status == 409 && r.body.contains("search"), "{}", r.body);
    assert_eq!(request(&node, "POST", "d", "/index?field=body", "").await.status, 409);
    ok(&node, "POST", "d", "/text-index?field=body", "").await;
    // Rebuilt from the log on restart; the shorter document ranks first.
    node = restart(node, &cfg, &app).await;
    assert_eq!(ranked(ok(&node, "GET", "d", &search("rust"), "").await), ["c", "b"]);
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs components/docs/build.sh"]
async fn docs_vector_nearest_neighbours() {
    let cfg = node_cfg("docs_vectors", 2);
    let app = app("docs", json!({}));
    let mut node = start(&cfg, &app);
    ok(&node, "POST", "d", "/vector-index?field=emb&dims=3", "").await;
    for (id, body) in [
        ("x", r#"{"emb":[1,0,0]}"#),
        ("y", r#"{"emb":[0,2,0]}"#),
        ("xy", r#"{"emb":[1,1,0]}"#),
        ("neg", r#"{"emb":[-1,0,0]}"#),
        ("short", r#"{"emb":[1,0]}"#),
        ("text", r#"{"emb":"no"}"#),
        ("zero", r#"{"emb":[0,0,0]}"#),
    ] {
        ok(&node, "PUT", "d", &format!("/doc?id={id}"), body).await;
    }
    let nearest = |v: &str, k: u32| format!("/nearest?field=emb&v={v}&k={k}");
    let ranked = |v: Value| {
        v.as_array()
            .unwrap()
            .iter()
            .map(|h| h[0].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    // Cosine similarity: direction, not length; only well-formed vectors.
    let hits = ok(&node, "GET", "d", &nearest("3,0.5,0", 10), "").await;
    assert_eq!(ranked(hits.clone()), ["x", "xy", "y", "neg"]);
    assert!((hits[3][1].as_f64().unwrap() + 0.986).abs() < 0.01, "{hits}");
    assert_eq!(
        ranked(ok(&node, "GET", "d", &nearest("0,1,0", 2), "").await),
        ["y", "xy"]
    );
    for bad in ["1,0", "0,0,0"] {
        let r = request(&node, "GET", "d", &nearest(bad, 2), "").await;
        assert_eq!(r.status, 409, "{bad}: {}", r.body);
    }
    // Replacing, deleting and restarting keep the index in step.
    ok(&node, "PUT", "d", "/doc?id=x", r#"{"emb":[0,0,1]}"#).await;
    ok(&node, "DELETE", "d", "/doc?id=y", "").await;
    assert_eq!(
        ranked(ok(&node, "GET", "d", &nearest("1,0,0", 10), "").await),
        ["xy", "x", "neg"]
    );
    node = restart(node, &cfg, &app).await;
    assert_eq!(
        ranked(ok(&node, "GET", "d", &nearest("1,0,0", 10), "").await),
        ["xy", "x", "neg"]
    );
    // One field, one kind of index.
    assert_eq!(
        request(&node, "POST", "d", "/vector-index?field=emb&dims=8", "")
            .await
            .status,
        409
    );
    assert_eq!(request(&node, "GET", "d", "/query?field=emb", "").await.status, 409);
    node.shutdown().await;
}
