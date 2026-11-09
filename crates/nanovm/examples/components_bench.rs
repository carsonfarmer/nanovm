//! Per-operation cost of the components in `components/`, through their test
//! apps on a real in-process node (no sockets). For each case: wall time and
//! process CPU (user+sys, from getrusage) per request, serially, and the
//! object's logical bytes on disk afterwards. Build the fixtures first.
//!
//!     cargo run --locked -p nanovm --example components_bench -- [filter] [--n 1000]
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, ensure};
use nanovm::config::{AppConfig, NodeConfig};
use nanovm::table::Node;

#[path = "../support/load.rs"]
#[allow(dead_code)]
mod load;

/// One measured case: `n` requests of `method path`, numbered `{i}` in the
/// path/body; `setup` requests run first, unmeasured.
struct Case {
    component: &'static str,
    name: &'static str,
    setup: &'static [(&'static str, &'static str, &'static str)],
    method: &'static str,
    path: &'static str,
    body: &'static str,
    /// Operations each request performs, to report per-op figures.
    ops: u64,
}

const CASES: &[Case] = &[
    // The composed KV provider's own test app, for reference.
    Case {
        component: "keyvalue",
        name: "reference: KV set 9 B / request",
        setup: &[],
        method: "GET",
        path: "/set",
        body: "",
        ops: 1,
    },
    Case {
        component: "keyvalue",
        name: "reference: KV get / request",
        setup: &[("GET", "/set", "")],
        method: "GET",
        path: "/get",
        body: "",
        ops: 1,
    },
    Case {
        component: "queue",
        name: "baseline: unknown route (404)",
        setup: &[],
        method: "GET",
        path: "/none",
        body: "",
        ops: 1,
    },
    Case {
        component: "queue",
        name: "append 1 x 100 B / request",
        setup: &[],
        method: "POST",
        path: "/append",
        body: B100,
        ops: 1,
    },
    Case {
        component: "queue",
        name: "read 1 / request",
        setup: &[("POST", "/bulk?count=1000&size=100&batch=100", "")],
        method: "GET",
        path: "/read?offset={i}&max=1",
        body: "",
        ops: 1,
    },
    Case {
        component: "queue",
        name: "next 1 (consume + commit position) / request",
        setup: &[("POST", "/bulk?count=1000&size=100&batch=100", "")],
        method: "POST",
        path: "/next?max=1",
        body: "",
        ops: 1,
    },
    Case {
        component: "queue",
        name: "append batch of 100 x 100 B / request (per message)",
        setup: &[],
        method: "POST",
        path: "/bulk?count=100&size=100&batch=100",
        body: "",
        ops: 100,
    },
    Case {
        component: "queue",
        name: "append 100 B x 1000 in one request (per append)",
        setup: &[],
        method: "POST",
        path: "/bulk?count=1000&size=100&batch=1",
        body: "",
        ops: 1000,
    },
];

const B100: &str =
    "0123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789";

fn cpu() -> f64 {
    let mut u = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    unsafe { libc::getrusage(libc::RUSAGE_SELF, u.as_mut_ptr()) };
    let u = unsafe { u.assume_init() };
    let t = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
    t(u.ru_utime) + t(u.ru_stime)
}

fn logical_bytes(dir: &Path) -> (u64, u64) {
    let (mut bytes, mut files) = (0, 0);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let md = e.metadata().unwrap();
            files += 1;
            if md.is_dir() {
                stack.push(e.path());
            } else {
                bytes += md.len();
            }
        }
    }
    (bytes, files)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let n: usize = args
        .iter()
        .position(|a| a == "--n")
        .map_or(1000, |i| args[i + 1].parse().unwrap());
    let filter = args
        .iter()
        .find(|a| !a.starts_with("--") && a.parse::<usize>().is_err())
        .cloned();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir: PathBuf = root.join(format!("target/components-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let cfg = NodeConfig {
        data_dir: dir.clone(),
        max_live_objects: 64,
        memory_reservation_mb: 64,
        ..NodeConfig::default()
    };
    cfg.validate()?;
    nanovm::engine::preflight(&cfg)?;
    println!("| component | case | requests | wall µs/req | CPU µs/req | CPU µs/op | bytes/op on disk |");
    println!("|---|---|---:|---:|---:|---:|---:|");
    for (k, case) in CASES.iter().enumerate() {
        if filter
            .as_deref()
            .is_some_and(|f| !case.component.contains(f) && !case.name.contains(f))
        {
            continue;
        }
        let fixture = match case.component {
            "keyvalue" => root.join("target/keyvalue/tests.wasm"),
            c => root.join("target/components").join(c).join("tests.wasm"),
        };
        let wasm = std::fs::read(fixture).with_context(|| format!("build components/{}/build.sh", case.component))?;
        let app = AppConfig {
            name: case.component.into(),
            digest: "bench".into(),
            env: Default::default(),
            config: Default::default(),
            worker: Default::default(),
            limits: serde_json::from_value(serde_json::json!({
                "memory_mb": 128, "cpu_ms": 60000, "request_timeout_ms": 120000, "disk_mb": 1024, "max_files": 100000
            }))?,
        };
        let node = Node::new(cfg.clone(), &app, &wasm, None)?;
        let id = format!("case{k}");
        for (method, path, body) in case.setup {
            let r = load::request(&node, method, &id, path, body).await;
            ensure!(r.status == 200, "setup {path}: {} {}", r.status, r.body);
        }
        // Warm the instance (and the compile cache) before measuring.
        let _ = load::request(&node, "GET", &id, "/none", "").await;
        let volume = dir.join("objects").join(case.component).join(&id);
        let (bytes0, _) = logical_bytes(&volume);
        let reqs = if case.ops > 1 {
            (n * 10 / case.ops as usize).max(3)
        } else {
            n
        };
        let (t0, c0) = (Instant::now(), cpu());
        for i in 0..reqs {
            let path = case.path.replace("{i}", &i.to_string());
            let r = load::request(&node, case.method, &id, &path, case.body).await;
            ensure!(r.status == 200 || path == "/none", "{path}: {} {}", r.status, r.body);
        }
        let (wall, used) = (t0.elapsed().as_secs_f64(), cpu() - c0);
        let (bytes1, _) = logical_bytes(&volume);
        let per_req = used / reqs as f64 * 1e6;
        println!(
            "| {} | {} | {} | {:.0} | {:.0} | {:.1} | {} |",
            case.component,
            case.name,
            reqs,
            wall / reqs as f64 * 1e6,
            per_req,
            per_req / case.ops as f64,
            match bytes1.checked_sub(bytes0) {
                Some(0) | None => "-".to_string(),
                Some(d) => format!("{:.1}", d as f64 / (reqs as u64 * case.ops) as f64),
            }
        );
        node.shutdown().await;
    }
    std::fs::remove_dir_all(dir)?;
    Ok(())
}
