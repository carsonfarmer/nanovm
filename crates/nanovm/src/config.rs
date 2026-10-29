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
        /// Upper bound on open WebSockets across all apps: each holds a file
        /// descriptor in the supervisor.
        max_sockets: usize = 10_000usize,
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
        /// The most a registry app gets: each of its limits is clamped to
        /// these. Egress entries are the app's own.
        registry_limits: Limits = Limits::default(),
        /// Worker limits for every registry app.
        registry_worker: WorkerLimits = WorkerLimits::default(),
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
    /// Values the guest reads through `wasi:config/store`.
    #[serde(default)]
    pub config: BTreeMap<String, String>,
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
        /// Open WebSockets across the app (held by the supervisor).
        max_sockets: usize = 1000usize,
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
        /// Open WebSockets per object.
        max_sockets: usize = 256usize,
        /// What a WebSocket may fall behind by, in bytes queued for its
        /// client, before it is dropped.
        socket_backlog_kb: u64 = 1024u64,
        max_outbound_concurrency: usize = 4usize,
        max_resources: usize = 10_000usize,
        /// Outbound HTTP allowlist: `host`, `host:port` or `*.suffix[:port]`.
        /// Empty denies all egress.
        egress: Vec<String> = Vec::new(),
    }
}

impl Limits {
    /// `self` with every numeric limit capped at `max`'s.
    pub fn clamped(&self, max: &Limits) -> Limits {
        let (mut v, max) = (serde_json::json!(self), serde_json::json!(max));
        for (k, x) in v.as_object_mut().expect("limits are a struct") {
            if let (Some(a), Some(b)) = (x.as_u64(), max[k].as_u64()) {
                *x = a.min(b).into();
            }
        }
        serde_json::from_value(v).expect("same shape")
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
        if self.registry_limits.memory_mb > self.memory_reservation_mb {
            bail!(
                "registry_limits.memory_mb {} exceeds memory_reservation_mb {}",
                self.registry_limits.memory_mb,
                self.memory_reservation_mb
            );
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str) -> AppConfig {
        toml::from_str(&format!("name = {name:?}\ncomponent = \"a.wasm\"")).unwrap()
    }

    /// Clamping caps every numeric limit at the maximum's, keeps the ones
    /// already under it, and leaves the egress list the app's own.
    #[test]
    fn clamped_caps_each_number_and_keeps_egress() {
        let mine = Limits {
            memory_mb: 1 << 20,
            cpu_ms: 1,
            max_sockets: usize::MAX,
            socket_backlog_kb: 0,
            egress: vec!["example.com".into()],
            ..Limits::default()
        };
        let max = Limits {
            memory_mb: 32,
            cpu_ms: 100,
            max_sockets: 8,
            egress: vec!["*.other".into()],
            ..Limits::default()
        };
        let c = mine.clamped(&max);
        assert_eq!(
            (c.memory_mb, c.cpu_ms, c.max_sockets, c.socket_backlog_kb),
            (32, 1, 8, 0)
        );
        assert_eq!(c.egress, ["example.com"]);
        assert_eq!(c.request_timeout_ms, Limits::default().request_timeout_ms);
        let d = Limits::default();
        assert_eq!(serde_json::json!(d.clamped(&d)), serde_json::json!(d));
    }

    /// Validation refuses memory limits beyond the reservation (equal fits)
    /// and app names that are invalid, reserved (`_…`) or repeated.
    #[test]
    fn validate_checks_memory_and_names() {
        let mut cfg = NodeConfig {
            apps: vec![app("a"), app("b")],
            ..NodeConfig::default()
        };
        cfg.validate().unwrap();

        cfg.registry_limits.memory_mb = cfg.memory_reservation_mb + 1;
        assert!(
            cfg.validate()
                .unwrap_err()
                .to_string()
                .contains("registry_limits.memory_mb")
        );
        cfg.registry_limits.memory_mb = cfg.memory_reservation_mb;
        cfg.apps[1].limits.memory_mb = cfg.memory_reservation_mb;
        cfg.validate().unwrap();
        cfg.apps[1].limits.memory_mb += 1;
        assert!(cfg.validate().unwrap_err().to_string().contains("app \"b\": memory_mb"));

        cfg.apps = vec![app("a"), app("a")];
        assert!(cfg.validate().unwrap_err().to_string().contains("duplicate app"));
        for bad in ["", "_nanovm", ".a", "a/b", "a b"] {
            cfg.apps = vec![app(bad)];
            assert!(
                cfg.validate().unwrap_err().to_string().contains("invalid app name"),
                "{bad:?}"
            );
        }
    }

    /// An empty file is the documented defaults; a partial table keeps the
    /// defaults of the keys it leaves out.
    #[test]
    fn toml_defaults() {
        let cfg: NodeConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.listen, "127.0.0.1:8080");
        assert_eq!(
            (cfg.max_live_objects, cfg.memory_reservation_mb, cfg.heavy_lanes),
            (1000, 128, 0)
        );
        assert!(cfg.apps.is_empty() && cfg.cgroup.is_none());
        assert_eq!(cfg.registry_worker.max_sockets, 1000);

        let cfg: NodeConfig = toml::from_str(
            "[registry_limits]\ncpu_ms = 9\n[[app]]\nname = \"a\"\ncomponent = \"a.wasm\"\n[app.limits]\ndisk_mb = 1\n",
        )
        .unwrap();
        assert_eq!((cfg.registry_limits.cpu_ms, cfg.registry_limits.memory_mb), (9, 64));
        let a = &cfg.apps[0];
        assert_eq!(
            (a.limits.disk_mb, a.limits.max_files, a.worker.max_sockets),
            (1, 1000, 1000)
        );
        assert!(a.static_dir.is_none() && a.env.is_empty());
    }

    /// A misspelt key anywhere is an error rather than silently a default,
    /// and an app needs a name and a component.
    #[test]
    fn toml_refuses_unknown_keys() {
        let app = "[[app]]\nname = \"a\"\ncomponent = \"a.wasm\"\n";
        for bad in [
            "listn = \"x\"".to_string(),
            "[registry_limits]\nmemory = 1".into(),
            "[registry_worker]\ncpu = 1".into(),
            format!("{app}static = \"x\""),
            format!("{app}[app.limits]\negres = []"),
            format!("{app}[app.worker]\nmem = 1"),
            "[[app]]\nname = \"a\"".into(),
            "[[app]]\ncomponent = \"a.wasm\"".into(),
        ] {
            assert!(toml::from_str::<NodeConfig>(&bad).is_err(), "{bad}");
        }
    }

    /// Loading resolves relative paths against the config file's directory,
    /// keeps absolute ones, and validates.
    #[test]
    fn load_resolves_relative_paths_and_validates() {
        let dir = std::env::temp_dir().join(format!("nanovm-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("n.toml");
        let app = "[[app]]\nname = \"a\"\ncomponent = \"c/a.wasm\"\nstatic_dir = \"/abs\"\n";
        std::fs::write(&path, app).unwrap();
        let cfg = NodeConfig::load(&path).unwrap();
        assert_eq!(cfg.data_dir, dir.join("nanovm-data"));
        assert_eq!(cfg.apps[0].component, dir.join("c/a.wasm"));
        assert_eq!(cfg.apps[0].static_dir.as_deref(), Some(Path::new("/abs")));
        assert_eq!(cfg.app_dirs("a")[1], dir.join("nanovm-data/tmp/a"));

        std::fs::write(&path, format!("{app}{app}")).unwrap();
        assert!(NodeConfig::load(&path).is_err(), "validated");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 0 means a share of the cores, at least one; anything else is kept.
    #[test]
    fn zero_means_a_share_of_the_cores() {
        assert_eq!(or_cores_over(3, 4), 3);
        assert!(or_cores_over(0, usize::MAX) >= 1);
        let cfg = NodeConfig {
            front_threads: 5,
            ..NodeConfig::default()
        };
        assert_eq!(
            (cfg.front_threads(), NodeConfig::default().heavy_lanes() >= 1),
            (5, true)
        );
    }
}
