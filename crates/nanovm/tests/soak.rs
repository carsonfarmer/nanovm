//! Soak test (D-51): churn and mixed traffic for `SOAK_SECS` seconds
//! (default 120), after which the process's RSS, open file descriptors and
//! memory mappings must be where they were after warming up. Ignored by
//! default; `scripts/soak.sh` runs it. Results: `notes/quality.md`.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use nanovm::config::{AppConfig, NodeConfig};
use nanovm::table::Node;

#[path = "../support/load.rs"]
#[allow(dead_code)]
mod load;

fn guest(name: &str) -> Vec<u8> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../guests/target/wasm32-wasip2/release").join(format!("{name}.wasm"));
    std::fs::read(&p).unwrap_or_else(|_| panic!("{} is missing: run scripts/build-guests.sh", p.display()))
}

/// A node for `name` (`counter` or `hostile`) with room for `live` objects.
fn node(name: &str, live: usize, limits: serde_json::Value) -> Arc<Node> {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("nodes").join(format!("soak-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = NodeConfig { data_dir: dir, max_live_objects: live, memory_reservation_mb: 64, dev: true, ..NodeConfig::default() };
    cfg.validate().unwrap();
    nanovm::engine::preflight(&cfg).unwrap();
    let app = AppConfig {
        name: name.into(), digest: "test".into(), env: Default::default(), config: Default::default(),
        limits: serde_json::from_value(limits).unwrap(), worker: Default::default(),
    };
    Node::new(cfg, &app, &guest(name), None).unwrap()
}

/// Unexpected answers, by request and answer.
type Odd = Arc<std::sync::Mutex<std::collections::BTreeMap<String, u64>>>;

#[derive(Clone, Copy, Debug)]
struct Sample {
    secs: u64,
    rss_kb: u64,
    fds: u64,
    maps: u64,
    requests: u64,
}

fn sample(t0: Instant, requests: u64) -> Sample {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let rss_kb = status.lines().find_map(|l| l.strip_prefix("VmRSS:")).unwrap().split_whitespace().next().unwrap().parse().unwrap();
    let fds = std::fs::read_dir("/proc/self/fd").unwrap().count() as u64;
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap().lines().count() as u64;
    Sample { secs: t0.elapsed().as_secs(), rss_kb, fds, maps, requests }
}

fn median(mut v: Vec<u64>) -> u64 {
    v.sort();
    v[v.len() / 2]
}

/// Small deterministic generator, so a run's traffic can be repeated.
fn next(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

/// One client's mix: counter increments, reads and key-value traffic over
/// 600 objects (64 live: constant eviction and restarts); on the hostile
/// app, commits and rollbacks, /tmp writes, echoes, and every kill the
/// host makes (CPU, memory, trap, resource table) over 200 objects.
async fn client(counter: Arc<Node>, hostile: Arc<Node>, mut seed: u64, stop: Arc<AtomicBool>, done: Arc<AtomicU64>, odd: Odd) {
    while !stop.load(Relaxed) {
        let r = next(&mut seed);
        let c = format!("c{}", r % 600);
        let h = format!("h{}", (r >> 16) % 200);
        let (node, id, method, path, body, ok): (&Arc<Node>, &str, &str, String, String, &[u16]) = match (r >> 32) % 100 {
            0..40 => (&counter, &c, "POST", "/incr".into(), String::new(), &[200]),
            40..55 => (&counter, &c, "GET", "/".into(), String::new(), &[200]),
            55..60 => (&counter, &c, "PUT", format!("/kv/k{}", r % 50), "v".repeat((r % 4096) as usize), &[204]),
            60..65 => (&counter, &c, "GET", format!("/kv/k{}", r % 50), String::new(), &[200, 404]),
            65..67 => (&counter, &c, "DELETE", format!("/kv/k{}", r % 50), String::new(), &[204]),
            67..75 => (&hostile, &h, "GET", format!("/commit?v={}", r % 1000), String::new(), &[200]),
            75..80 => (&hostile, &h, "GET", format!("/commit?v={}&then=error", r % 1000), String::new(), &[500]),
            80..84 => (&hostile, &h, "GET", "/fill?mount=tmp&mb=1".into(), String::new(), &[200, 507]),
            84..92 => (&hostile, &h, "POST", "/echo".into(), "x".repeat((r % 65536) as usize), &[200]),
            92..94 => (&hostile, &h, "GET", "/spin".into(), String::new(), &[500]),
            94..96 => (&hostile, &h, "GET", "/alloc?mb=1024".into(), String::new(), &[500]),
            96..98 => (&hostile, &h, "GET", "/panic".into(), String::new(), &[500]),
            _ => (&hostile, &h, "GET", "/leak".into(), String::new(), &[500]),
        };
        let reply = load::request(node, method, id, &path, &body).await;
        // Busy (one object, several clients) is fine, and so, on a loaded
        // host, is a budget running out; anything else unexpected isn't.
        let tolerated = matches!(reply.error.as_deref(), Some("busy" | "cpu" | "timeout"));
        if !ok.contains(&reply.status) && !tolerated {
            let path = path.split('?').next().unwrap_or_default();
            let key = format!("{method} {path}: {} {:?} {}", reply.status, reply.error, reply.body.chars().take(160).collect::<String>());
            *odd.lock().unwrap().entry(key).or_default() += 1;
        }
        done.fetch_add(1, Relaxed);
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "long; scripts/soak.sh runs it (SOAK_SECS, default 120)"]
async fn resources_stay_flat_under_churn_and_mixed_traffic() {
    let secs: u64 = std::env::var("SOAK_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(120);
    let counter = node("counter", 64, serde_json::json!({
        "memory_mb": 16, "cpu_ms": 1000, "request_timeout_ms": 5000, "disk_mb": 1, "max_files": 100,
    }));
    let hostile = node("hostile", 32, serde_json::json!({
        "memory_mb": 32, "cpu_ms": 300, "request_timeout_ms": 2000, "post_return_timeout_ms": 500,
        "disk_mb": 8, "max_files": 200, "max_resources": 2000,
    }));
    let (stop, done, odd) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicU64::new(0)), Odd::default());
    let clients: Vec<_> = (0..6u64)
        .map(|i| tokio::spawn(client(counter.clone(), hostile.clone(), 0x9e37_79b9_7f4a_7c15 ^ (i + 1) * 0x1234_5678, stop.clone(), done.clone(), odd.clone())))
        .collect();
    let t0 = Instant::now();
    let every = Duration::from_secs((secs / 60).max(2));
    let mut samples = vec![];
    let csv = std::env::var("SOAK_CSV").ok();
    let mut out = String::from("secs,rss_kb,fds,maps,requests,live_counter,live_hostile\n");
    while t0.elapsed() < Duration::from_secs(secs) {
        tokio::time::sleep(every).await;
        assert!(clients.iter().all(|c| !c.is_finished()), "a client stopped");
        let s = sample(t0, done.load(Relaxed));
        let line = format!("{},{},{},{},{},{},{}", s.secs, s.rss_kb, s.fds, s.maps, s.requests, counter.live_objects(), hostile.live_objects());
        eprintln!("soak {line}");
        out += &(line + "\n");
        if let Some(path) = &csv {
            std::fs::write(path, &out).unwrap();
        }
        samples.push(s);
    }
    stop.store(true, Relaxed);
    for c in clients {
        c.await.unwrap();
    }
    // Compare the second quarter (warm: every pool slot and cache touched)
    // with the last quarter.
    let q = samples.len() / 4;
    assert!(q >= 2, "too short a run to judge: {} samples", samples.len());
    let (warm, last) = (&samples[q..2 * q], &samples[samples.len() - q..]);
    let m = |s: &[Sample], f: fn(&Sample) -> u64| median(s.iter().map(f).collect());
    let (rss, fds, maps) = (
        (m(warm, |s| s.rss_kb), m(last, |s| s.rss_kb)),
        (m(warm, |s| s.fds), m(last, |s| s.fds)),
        (m(warm, |s| s.maps), m(last, |s| s.maps)),
    );
    let total = samples.last().unwrap().requests;
    eprintln!("soak: {total} requests in {secs} s; RSS {} -> {} MiB, fds {} -> {}, mappings {} -> {}",
        rss.0 >> 10, rss.1 >> 10, fds.0, fds.1, maps.0, maps.1);
    let odd = odd.lock().unwrap();
    eprintln!("soak: unexpected answers {odd:#?}");
    assert!(odd.values().sum::<u64>() * 1000 <= total, "more than 0.1% unexpected answers");
    assert!(rss.1 <= rss.0 + (rss.0 / 10).max(32 << 10), "RSS grew from {} to {} KiB", rss.0, rss.1);
    assert!(fds.1 <= fds.0 + 32, "open fds grew from {} to {}", fds.0, fds.1);
    assert!(maps.1 <= maps.0 + 100, "mappings grew from {} to {}", maps.0, maps.1);
}
