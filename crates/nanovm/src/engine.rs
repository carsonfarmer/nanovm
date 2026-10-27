//! The shared wasmtime engine: pooling allocator sized for the node, the
//! epoch ticker, and a content-addressed `.cwasm` compile cache.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

use anyhow::Context;
use sha2::{Digest, Sha256};
use wasmtime::component::Component;
use wasmtime::{Config, Engine, InstanceAllocationStrategy, PoolingAllocationConfig};

use crate::config::NodeConfig;

pub fn build(cfg: &NodeConfig) -> anyhow::Result<Engine> {
    let mut c = Config::new();
    c.wasm_component_model_async(true);
    c.epoch_interruption(true);
    c.max_wasm_stack(512 << 10);
    c.async_stack_size(1 << 20);
    c.memory_init_cow(true);

    // Small per-memory reservations keep 10k+ slots inside the address space;
    // the cost is explicit bounds checks instead of guard-page elision.
    let reservation = cfg.memory_reservation_mb << 20;
    c.memory_reservation(reservation);
    c.memory_guard_size(64 << 10);
    c.memory_reservation_for_growth(0);

    // `Node::slots` keeps live stores to `max_live_objects`, one component
    // instance each.
    let n = cfg.max_live_objects as u32 + 32;
    let mut pool = PoolingAllocationConfig::default();
    pool.total_component_instances(n);
    pool.total_core_instances(n * 4);
    pool.total_memories(n * 2);
    pool.total_tables(n * 4);
    pool.total_stacks(n * 2);
    pool.max_memory_size(reservation as usize);
    pool.max_memories_per_component(4);
    pool.max_tables_per_component(8);
    pool.max_core_instances_per_component(16);
    pool.table_elements(20_000);
    pool.max_unused_warm_slots(n.min(1000));
    c.allocation_strategy(InstanceAllocationStrategy::Pooling(pool));

    Engine::new(&c).map_err(Into::into)
}

/// File descriptors a live object holds: its `/data` and `/tmp` preopens.
/// `/app` is one shared fd per app; files a guest opens come on top.
pub const FDS_PER_OBJECT: u64 = 2;
/// Memory mappings per pool slot, measured on Linux: about 4 per slot once
/// the pool is created (stacks and their guard pages) and 4 more per live
/// instance (its memory and tables made accessible).
pub const MAPS_PER_OBJECT: u64 = 8;

/// Checks that the process can actually hold `max_live_objects`, raising the
/// soft fd limit to the hard limit first. Without this a node that is too
/// big for its host starts fine and then fails instantiations at runtime
/// with `EMFILE` or `ENOMEM`.
pub fn preflight(cfg: &NodeConfig) -> anyhow::Result<()> {
    let n = cfg.max_live_objects as u64 + 32;
    let fds = raise_nofile()?;
    let need_fds = n * FDS_PER_OBJECT + 1024;
    anyhow::ensure!(
        fds >= need_fds,
        "max_live_objects = {} needs about {need_fds} file descriptors, but RLIMIT_NOFILE is {fds}; \
         raise the limit (e.g. `ulimit -n {need_fds}` or LimitNOFILE= in the unit) or lower max_live_objects",
        cfg.max_live_objects
    );
    if let Ok(text) = std::fs::read_to_string("/proc/sys/vm/max_map_count") {
        let maps: u64 = text.trim().parse().unwrap_or(u64::MAX);
        let need_maps = n * MAPS_PER_OBJECT + 4096;
        anyhow::ensure!(
            maps >= need_maps,
            "max_live_objects = {} needs about {need_maps} memory mappings, but vm.max_map_count is {maps}; \
             raise it (`sysctl -w vm.max_map_count={need_maps}`) or lower max_live_objects",
            cfg.max_live_objects
        );
    }
    Ok(())
}

/// Raises the soft `RLIMIT_NOFILE` to the hard limit and returns the result.
fn raise_nofile() -> anyhow::Result<u64> {
    let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: `lim` is a valid out-pointer for getrlimit/setrlimit.
    unsafe {
        anyhow::ensure!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) == 0, "getrlimit failed");
        // macOS rejects values above OPEN_MAX even when the hard limit is
        // RLIM_INFINITY, so fall back to the current limit there.
        if lim.rlim_cur < lim.rlim_max {
            let want = libc::rlimit { rlim_cur: lim.rlim_max, rlim_max: lim.rlim_max };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &want) == 0 {
                lim.rlim_cur = lim.rlim_max;
            }
        }
    }
    Ok(lim.rlim_cur as u64)
}

/// Advances the engine epoch every `tick` on a dedicated thread.
pub fn start_ticker(engine: &Engine, tick: Duration) {
    let engine = engine.weak();
    std::thread::Builder::new()
        .name("nanovm-epoch".into())
        .spawn(move || {
            while let Some(e) = engine.upgrade() {
                e.increment_epoch();
                drop(e);
                std::thread::sleep(tick);
            }
        })
        .expect("spawn epoch thread");
}

/// Loads a component, reusing a precompiled artifact keyed by the component's
/// hash and the engine's compatibility hash.
pub fn load_component(engine: &Engine, path: &Path, cache_dir: &Path) -> anyhow::Result<Component> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let digest = Sha256::digest(&bytes);
    let mut h = DefaultHasher::new();
    engine.precompile_compatibility_hash().hash(&mut h);
    let key = format!("{}-{:016x}.cwasm", hex(&digest[..16]), h.finish());
    let cached = cache_dir.join(key);

    if cached.exists() {
        // SAFETY: the cache directory is owned by this node and only ever
        // written by `precompile_component` below.
        match unsafe { Component::deserialize_file(engine, &cached) } {
            Ok(c) => return Ok(c),
            Err(_) => {
                let _ = std::fs::remove_file(&cached);
            }
        }
    }
    let compiled = engine.precompile_component(&bytes)?;
    std::fs::create_dir_all(cache_dir)?;
    // Unique per writer: nodes and apps may share a cache and race here, and
    // a reader must only ever see a complete file.
    static WRITER: AtomicU64 = AtomicU64::new(0);
    let tmp = cached.with_extension(format!("tmp.{}.{}", std::process::id(), WRITER.fetch_add(1, Relaxed)));
    std::fs::write(&tmp, &compiled)?;
    std::fs::rename(&tmp, &cached)?;
    // SAFETY: as above.
    Ok(unsafe { Component::deserialize_file(engine, &cached)? })
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
