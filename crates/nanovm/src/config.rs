//! Node and per-app configuration, loaded from TOML.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;

/// Declares a config struct in which every field has a default, so the
/// default is written once, next to the field.
macro_rules! config {
    ($(#[$m:meta])* pub struct $name:ident { $($(#[$fm:meta])* $f:ident: $t:ty = $d:expr,)* }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Deserialize, serde::Serialize)]
        #[serde(deny_unknown_fields, default)]
        pub struct $name { $($(#[$fm])* pub $f: $t,)* }

        impl Default for $name {
            fn default() -> Self {
                $name { $($f: $d.into(),)* }
            }
        }
    };
}

config! {
    pub struct NodeConfig {
        /// Address the HTTP front end listens on.
        listen: String = "127.0.0.1:8080",
        /// Root for per-object volumes, scratch space and the compile cache.
        data_dir: PathBuf = "nanovm-data",
        /// Upper bound on objects with a live instance; sizes the pooling allocator.
        max_live_objects: usize = 1000usize,
        /// Virtual address space reserved per linear memory. Every app's
        /// `memory_mb` must fit inside it.
        memory_reservation_mb: u64 = 128u64,
        /// Epoch tick; the granularity of CPU-budget checks and guest yields.
        epoch_tick_ms: u64 = 5u64,
        /// Requests that have used more than `HEAVY_AFTER` of CPU take turns on
        /// this many cores, so short requests always find a free one. 0 means
        /// half the cores.
        heavy_lanes: usize = 0usize,
        /// Threads serving the HTTP front end, each with its own runtime,
        /// listener and connections to the workers. The supervisor needs about
        /// a fifth of a request's CPU, so 0 means a quarter of the cores.
        front_threads: usize = 0usize,
        /// When the supervisor runs as root, app `i`'s worker runs as uid and
        /// gid `jail_uid_base + i`.
        jail_uid_base: u32 = 200_000u32,
        /// A delegated cgroup v2 directory, with the memory, cpu and pids
        /// controllers enabled for its children. Each worker is placed in
        /// `<cgroup>/<app>` with that app's `[app.worker]` limits.
        cgroup: Option<PathBuf> = None::<PathBuf>,
        #[serde(rename = "app")]
        apps: Vec<AppConfig> = Vec::new(),
    }
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub name: String,
    /// Path to the wasip3 HTTP component.
    pub component: PathBuf,
    /// Host directory mounted read-only at `/app`.
    pub static_dir: Option<PathBuf>,
    /// Extra environment variables visible to the guest.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub worker: WorkerLimits,
}

config! {
    /// Limits on an app's whole worker process, applied through its cgroup.
    /// 0 leaves a limit unset.
    pub struct WorkerLimits {
        memory_mb: u64 = 0u64,
        cpus: f64 = 0.0f64,
        pids: u64 = 0u64,
    }
}

config! {
    /// Per-object limits. Every live object of an app gets its own copy.
    pub struct Limits {
        memory_mb: u64 = 64u64,
        cpu_ms: u64 = 200u64,
        request_timeout_ms: u64 = 5_000u64,
        post_return_timeout_ms: u64 = 5_000u64,
        idle_timeout_ms: u64 = 30_000u64,
        max_request_body_kb: u64 = 1024u64,
        max_response_body_kb: u64 = 8192u64,
        /// Disk budget shared by `/data` and `/tmp`.
        disk_mb: u64 = 64u64,
        max_files: u64 = 1000u64,
        max_queue: usize = 64usize,
        max_outbound_concurrency: usize = 4usize,
        max_resources: usize = 10_000usize,
        /// Outbound HTTP allowlist: `host`, `host:port` or `*.suffix[:port]`.
        /// Empty denies all egress.
        egress: Vec<String> = Vec::new(),
    }
}

impl NodeConfig {
    pub fn front_threads(&self) -> usize {
        or_cores_over(self.front_threads, 4)
    }

    pub fn heavy_lanes(&self) -> usize {
        or_cores_over(self.heavy_lanes, 2)
    }

    /// An app's writable directories: object volumes, scratch space and its
    /// compile cache.
    pub fn app_dirs(&self, app: &str) -> [PathBuf; 3] {
        ["objects", "tmp", "cache"].map(|d| self.data_dir.join(d).join(app))
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: NodeConfig = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        // Relative paths are relative to the config file.
        let base = path.parent().unwrap_or(Path::new("."));
        cfg.resolve(base);
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn resolve(&mut self, base: &Path) {
        let fix = |p: &mut PathBuf| {
            if p.is_relative() {
                *p = base.join(&*p);
            }
        };
        fix(&mut self.data_dir);
        for app in &mut self.apps {
            fix(&mut app.component);
            if let Some(d) = &mut app.static_dir {
                fix(d);
            }
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let mut seen = std::collections::HashSet::new();
        for app in &self.apps {
            if !crate::table::valid_name(&app.name) || app.name.starts_with('_') {
                bail!("invalid app name {:?}", app.name);
            }
            if !seen.insert(&app.name) {
                bail!("duplicate app {:?}", app.name);
            }
            if app.limits.memory_mb > self.memory_reservation_mb {
                bail!(
                    "app {:?}: memory_mb {} exceeds memory_reservation_mb {}",
                    app.name,
                    app.limits.memory_mb,
                    self.memory_reservation_mb
                );
            }
        }
        Ok(())
    }
}

/// `n`, or for 0 the number of cores divided by `div` (at least 1).
fn or_cores_over(n: usize, div: usize) -> usize {
    match n {
        0 => std::thread::available_parallelism().map_or(1, |c| (c.get() / div).max(1)),
        n => n,
    }
}
