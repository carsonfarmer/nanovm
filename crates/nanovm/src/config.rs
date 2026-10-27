//! Node and per-app configuration, loaded from TOML.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    /// Address the HTTP front end listens on.
    #[serde(default = "d_listen")]
    pub listen: String,
    /// Root for per-object volumes, scratch space and the compile cache.
    #[serde(default = "d_data_dir")]
    pub data_dir: PathBuf,
    /// Upper bound on objects with a live instance; sizes the pooling allocator.
    #[serde(default = "d_max_live")]
    pub max_live_objects: usize,
    /// Virtual address space reserved per linear memory. Every app's
    /// `memory_mb` must fit inside it.
    #[serde(default = "d_reservation")]
    pub memory_reservation_mb: u64,
    /// Epoch tick; the granularity of CPU-budget checks and guest yields.
    #[serde(default = "d_tick")]
    pub epoch_tick_ms: u64,
    /// Requests that have used more than `HEAVY_AFTER` of CPU take turns on
    /// this many cores, so short requests always find a free one. 0 means
    /// half the cores.
    #[serde(default)]
    pub heavy_lanes: usize,
    /// Enables `/_nanovm/demo/*` load-generation endpoints.
    #[serde(default)]
    pub demo: bool,
    /// Directory served at `/_nanovm/` (the dashboard).
    #[serde(default)]
    pub dashboard: Option<PathBuf>,
    #[serde(default, rename = "app")]
    pub apps: Vec<AppConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub name: String,
    /// Path to the wasip3 HTTP component.
    pub component: PathBuf,
    /// Host directory mounted read-only at `/app`.
    #[serde(default)]
    pub static_dir: Option<PathBuf>,
    /// Extra environment variables visible to the guest.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub limits: Limits,
}

/// Per-object limits. Every live object of an app gets its own copy.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    pub memory_mb: u64,
    pub cpu_ms: u64,
    pub request_timeout_ms: u64,
    pub post_return_timeout_ms: u64,
    pub idle_timeout_ms: u64,
    pub max_request_body_kb: u64,
    pub max_response_body_kb: u64,
    pub tmp_mb: u64,
    pub data_mb: u64,
    pub max_files: u64,
    pub max_file_mb: u64,
    pub max_queue: usize,
    pub max_outbound_concurrency: usize,
    pub max_resources: usize,
    /// Outbound HTTP allowlist: `host`, `host:port` or `*.suffix[:port]`.
    /// Empty denies all egress.
    pub egress: Vec<String>,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            memory_mb: 64,
            cpu_ms: 200,
            request_timeout_ms: 5_000,
            post_return_timeout_ms: 5_000,
            idle_timeout_ms: 30_000,
            max_request_body_kb: 1024,
            max_response_body_kb: 8192,
            tmp_mb: 16,
            data_mb: 64,
            max_files: 1000,
            max_file_mb: 16,
            max_queue: 64,
            max_outbound_concurrency: 4,
            max_resources: 10_000,
            egress: Vec::new(),
        }
    }
}

fn d_listen() -> String {
    "127.0.0.1:8080".into()
}
fn d_data_dir() -> PathBuf {
    "nanovm-data".into()
}
fn d_max_live() -> usize {
    1000
}
fn d_reservation() -> u64 {
    128
}
fn d_tick() -> u64 {
    5
}

impl NodeConfig {
    pub fn heavy_lanes(&self) -> usize {
        match self.heavy_lanes {
            0 => std::thread::available_parallelism().map_or(1, |n| (n.get() / 2).max(1)),
            n => n,
        }
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
        if let Some(d) = &mut self.dashboard {
            fix(d);
        }
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
