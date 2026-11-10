//! The shared wasmtime engine: pooling allocator sized for the node, the
//! epoch ticker, and a content-addressed `.cwasm` compile cache.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

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
    c.memory_guard_size(GUARD);
    c.memory_reservation_for_growth(0);

    // Every admitted component can use its full per-component allowance.
    // Composed providers add memories, tables and adapter core instances.
    let n = cfg.max_live_objects as u32 + 32;
    let (cores, memories, tables) = (16, MEMORIES_PER_OBJECT as u32, 8);
    let mut pool = PoolingAllocationConfig::default();
    pool.total_component_instances(n);
    pool.total_core_instances(n * cores);
    pool.total_memories(n * memories);
    pool.total_tables(n * tables);
    pool.total_stacks(n * 2);
    pool.max_memory_size(reservation as usize);
    pool.max_memories_per_component(memories);
    pool.max_tables_per_component(tables);
    pool.max_core_instances_per_component(cores);
    pool.table_elements(20_000);
    pool.max_unused_warm_slots(n.min(1000));
    c.allocation_strategy(InstanceAllocationStrategy::Pooling(pool));

    Engine::new(&c).map_err(Into::into)
}

/// Linear memories per component instance, each reserved up front.
pub const MEMORIES_PER_OBJECT: u64 = 4;
/// The guard after each linear memory.
const GUARD: u64 = 64 << 10;

/// File descriptors a live object holds: its `/data` and `/tmp` preopens.
/// Files a guest opens come on top.
pub const FDS_PER_OBJECT: u64 = 2;
/// Conservative Linux mapping allowance: stacks/guards plus up to four
/// memories and eight tables per composed instance, including protection splits.
pub const MAPS_PER_OBJECT: u64 = 24;

/// Checks that the process can actually hold `max_live_objects`, raising the
/// soft fd limit to the hard limit first. Without this a node that is too
/// big for its host starts fine and then fails instantiations at runtime
/// with `EMFILE` or `ENOMEM`, or its engine with "failed to create memory
/// pool mapping".
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
    // Building the engine reserves every slot's memories, stacks and tables
    // (about 3.3 MiB) at once (N-671); the rest of the process gets 1 GiB.
    let need_as = n * (MEMORIES_PER_OBJECT * ((cfg.memory_reservation_mb << 20) + GUARD) + (4 << 20)) + (1 << 30);
    let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: `lim` is a valid pointer for getrlimit.
    if unsafe { libc::getrlimit(libc::RLIMIT_AS, &mut lim) } == 0 && lim.rlim_cur != libc::RLIM_INFINITY {
        let (need, have) = (need_as.div_ceil(1 << 30), lim.rlim_cur as u64 >> 20);
        anyhow::ensure!(
            lim.rlim_cur as u64 >= need_as,
            "max_live_objects = {} with memory_reservation_mb = {} reserves about {need} GiB of address space, \
             but RLIMIT_AS is {have} MiB; remove the limit (`ulimit -v unlimited` or LimitAS=infinity in the unit) \
             or lower max_live_objects or memory_reservation_mb",
            cfg.max_live_objects, cfg.memory_reservation_mb
        );
    }
    Ok(())
}

/// Raises the soft `RLIMIT_NOFILE` to the hard limit and returns the result.
pub(crate) fn raise_nofile() -> anyhow::Result<u64> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `lim` is a valid pointer for getrlimit/setrlimit. macOS
    // rejects values above OPEN_MAX even when the hard limit is
    // RLIM_INFINITY; then the soft limit simply stays where it was.
    unsafe {
        anyhow::ensure!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) == 0, "getrlimit failed");
        let soft = lim.rlim_cur;
        lim.rlim_cur = lim.rlim_max;
        if libc::setrlimit(libc::RLIMIT_NOFILE, &lim) != 0 {
            lim.rlim_cur = soft;
        }
    }
    Ok(lim.rlim_cur as u64)
}

/// Advances the engine epoch every `tick` on a dedicated thread. CPU budgets
/// and guest yields depend on it, so if the thread ever dies the process
/// aborts rather than run guests unlimited.
pub fn start_ticker(engine: &Engine, tick: Duration) {
    let engine = engine.weak();
    std::thread::Builder::new()
        .name("nanovm-epoch".into())
        .spawn(move || {
            let ticked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                while let Some(e) = engine.upgrade() {
                    e.increment_epoch();
                    drop(e);
                    std::thread::sleep(tick);
                }
            }));
            if ticked.is_err() {
                std::process::abort();
            }
        })
        .expect("spawn epoch thread");
}

/// Compiles a component, reusing a precompiled artifact keyed by the
/// component's hash and the engine's compatibility hash.
pub fn load_component(engine: &Engine, bytes: &[u8], cache_dir: &Path) -> anyhow::Result<Component> {
    let digest = Sha256::digest(bytes);
    let mut h = DefaultHasher::new();
    engine.precompile_compatibility_hash().hash(&mut h);
    let key = format!("{digest:x}-{:016x}.cwasm", h.finish());
    let cached = cache_dir.join(key);

    // SAFETY: the cache directory belongs to this app and is only ever
    // written by `precompile_component` below, in this app's process. A
    // missing or stale file fails here and is replaced by the rename below.
    if let Ok(c) = unsafe { Component::deserialize_file(engine, &cached) } {
        return Ok(c);
    }
    let compiled = engine.precompile_component(bytes)?;
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
