//! Node and per-app configuration, loaded from TOML.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};
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
        /// Optional local committed-version capture. Not remote replication.
        capture: Option<CaptureConfig> = None::<CaptureConfig>,
        /// Upper bound on objects with a live instance; sizes the pooling allocator.
        max_live_objects: usize = 1000usize,
        /// The most objects one app may have on this node: an object counts
        /// from its first committed change until its volume is empty again
        /// (or an operator deletes it). A new one past it is refused (507
        /// `object-limit`), as is one when the disk has less free than its
        /// quota (507 `disk-full`).
        max_objects_per_app: u64 = 10_000u64,
        /// The proxy's private endpoint: committed publishes go to its
        /// `/publish`, and with `leases`, calls to other objects to its
        /// `/objects/…`, which places them.
        proxy: String = "http://127.0.0.1:8444",
        /// With more than one node: where the nodes keep their leases on
        /// objects, an object store URL (`s3://bucket/prefix`; credentials
        /// from the environment, as `AWS_ACCESS_KEY_ID` and so on). Empty:
        /// this is the only node, and every object is its own (see `lease`).
        leases: String = "",
        /// This node as the proxy lists it (`host:port`), naming it as a
        /// lease holder. Every node's must differ.
        name: String = "",
        /// How long a node's leases outlive its last renewal: the longest an
        /// object is unavailable after its node stops (see `lease`). Each
        /// node's session record carries its own, which other nodes wait
        /// out before taking its objects, so nodes may differ.
        lease_ms: u64 = 10_000u64,
        /// OTLP/HTTP collector base URL. Only the supervisor connects to it.
        telemetry_endpoint: String = "http://127.0.0.1:4318",
        /// Virtual address space reserved per linear memory: no memory grows
        /// past it, whatever an app's `memory_mb`.
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
        /// The most an app gets: each of its limits is clamped to these, and
        /// its egress reaches only what both its list and this `egress`
        /// allow (by default nothing).
        limits: Limits = Limits::default(),
        /// Every app's worker limits.
        worker: WorkerLimits = WorkerLimits::default(),
        /// The largest component the node will download.
        max_component_mb: u64 = 64u64,
        /// Upper bound on running app workers. A new app takes the slot of
        /// the least recently used idle one (no requests), or is
        /// refused when none is idle.
        max_apps: usize = 100usize,
        /// How long a registry fetch (an app's load, or a check of a new
        /// policy) may take in all, however slowly its registry answers.
        load_timeout_ms: u64 = 60_000u64,
        /// For local development only: apps need no signature, registries
        /// and egress may be on private addresses, and loopback registries
        /// are plain HTTP. Allowed only in a build with the `dev` feature and
        /// with a loopback `listen`.
        dev: bool = false,
        /// A directory only this user can use, holding each app's secrets
        /// as `<app>.json` (see `secrets`). None: apps get none.
        secrets: Option<PathBuf> = None::<PathBuf>,
        /// Where workers run, each in a cgroup v2 of its own with the
        /// `[worker]` limits: `"auto"` makes the supervisor's own cgroup a
        /// subtree for them (systemd: `Delegate=yes`) and refuses to start
        /// where it can't; a directory names a delegated one, used by this
        /// node alone, with the cpu, memory and pids controllers enabled for
        /// its children; `"off"` runs workers without those limits.
        cgroup: String = "auto",
        /// What clients may ask of `listen` when they reach it directly.
        clients: ClientLimits = ClientLimits::default(),
        /// Bytes all of an app's objects' volumes may hold, in MiB: past it
        /// that app's writes are refused. 0 is a quarter of `data_dir`'s
        /// filesystem.
        app_disk_mb: u64 = 0u64,
        /// Free space kept on `data_dir`'s filesystem, in MiB: below it,
        /// writes that need more space are refused for every app, so
        /// commits, rollbacks and deletes always find room. 0 is a
        /// twentieth of the filesystem, at most 1 GiB.
        disk_reserve_mb: u64 = 0u64,
    }
}

/// An app as its worker runs it: from its registry config, within the
/// node's limits. The supervisor sends it whole in a worker's spec.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct AppConfig {
    pub name: String,
    /// Manifest digest assigned by the supervisor, not the guest manifest.
    pub digest: String,
    /// Extra environment variables visible to the guest.
    pub env: BTreeMap<String, String>,
    /// Values the guest reads through `wasi:config/store`.
    pub config: BTreeMap<String, String>,
    /// The operator's secrets for the app, read with (and over) `config`.
    pub secrets: crate::secrets::Secrets,
    pub limits: Limits,
    pub worker: WorkerLimits,
}

config! {
    /// `nanovm proxy`: the public front door (see `proxy`).
    pub struct ProxyConfig {
        /// Address the proxy listens on.
        listen: String = "0.0.0.0:8443",
        /// The nodes it places objects on (`host:port`, each as the node's
        /// `name`).
        nodes: Vec<String> = vec!["127.0.0.1:8080".to_string()],
        /// How long a request may wait for its object's node: for a node that
        /// stopped to lose its leases (its `lease_ms`) and another to take
        /// over. It must exceed every node's `lease_ms`, with the time a
        /// takeover takes to restore an object; the proxy can't check that,
        /// as each node has its own. More than the connect timeout (1 s).
        failover_ms: u64 = 30_000u64,
        /// Private control listener; only nodes may reach it.
        publish_listen: String = "127.0.0.1:8444",
        /// OTLP/HTTP Collector base URL for front-door request events.
        telemetry_endpoint: String = "http://127.0.0.1:4318",
        max_sockets: usize = 10_000usize,
        max_sockets_per_app: usize = 1000usize,
        max_sockets_per_object: usize = 256usize,
        max_request_body_kb: u64 = 1024u64,
        max_response_body_kb: u64 = 8192u64,
        socket_backlog_kb: u64 = 1024u64,
        /// A DNS-over-HTTPS resolver (the JSON API) asked for each host's
        /// `_nanovm` TXT record.
        resolver: String = "https://cloudflare-dns.com/dns-query",
        /// How long a tag's digest is reused before asking the registry again.
        tag_ttl_s: u64 = 30u64,
        /// For local development only: registries may be on private
        /// addresses, and loopback ones are plain HTTP (as for the node).
        dev: bool = false,
        /// The hosts served: `name`, or `*.name` for any name under it. Any
        /// other host is refused (403) before its record is looked up.
        hosts: Vec<String> = Vec::new(),
        /// The identities that may deploy, `<issuer> <subject>`, the subject
        /// exact or ending in `*` (then any subject that starts with what
        /// comes before it). A record's `trust=` entries are cut to these,
        /// and a record left with none is refused (403).
        signers: Vec<String> = Vec::new(),
        /// What clients may ask of `listen`.
        clients: ClientLimits = ClientLimits::default(),
    }
}

config! {
    /// What one client, and one app, may ask of a public listener (see
    /// `gate`). 0 turns a limit off.
    pub struct ClientLimits {
        /// Requests a second from one client address (an IPv6 /64), with
        /// bursts of twice that.
        rate: u32 = 100u32,
        /// Requests a second to one app, with bursts of twice that.
        app_rate: u32 = 1000u32,
        /// Connections open at once from one client address.
        connections_per_client: usize = 256usize,
        /// Connections open at once in all: past it, new ones wait.
        connections: usize = 10_000usize,
        /// Peers that forward other clients' requests (a TLS terminator, the
        /// proxy), as addresses or CIDR ranges. Their own requests and
        /// connections aren't limited, and a request's client is the last
        /// address in its `X-Forwarded-For` that isn't one of theirs.
        trusted: Vec<String> = vec!["127.0.0.1".to_string(), "::1".to_string()],
    }
}

config! {
    /// Each object's committed versions, captured here and shipped to `store`.
    pub struct CaptureConfig {
        /// The most an object's versions may take here, in MiB: those not
        /// shipped yet (all of them while the store is down), and the last.
        /// Writes to the object are refused beyond it, never versions
        /// dropped. Size the disk for it beside the objects' `disk_mb`.
        max_mb: u64 = 256u64,
        /// Every this many versions, one holds the whole volume; the rest
        /// hold what a commit changed.
        snapshot_every: u64 = 16u64,
        /// Where published versions are shipped, then removed here but the
        /// last: an object store URL, as `leases` takes (`s3://bucket/prefix`,
        /// `file:///path`; credentials and region from `AWS_*`). None keeps
        /// them all here.
        store: Option<String> = None::<String>,
        /// Acknowledge a commit once its version counts in the store (its
        /// head names it), not once it is durable here. Unset in a config
        /// file, it is on for a node with `leases`: a node taking an object
        /// over restores it from the store, so a write answered only here
        /// would be lost. Off for one node.
        remote_ack: bool = false,
    }
}

config! {
    /// Limits on an app's whole worker process, applied through its cgroup.
    /// The defaults leave the rest of the host room: a quarter of its memory
    /// and all its cores but one. 0 leaves a limit unset.
    pub struct WorkerLimits {
        memory_mb: u64 = host_memory_mb() / 4,
        cpus: f64 = or_cores_over(0, 1).saturating_sub(1).max(1) as f64,
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

impl Limits {
    /// `self` with every numeric limit capped at `max`'s, and those it
    /// lowered: each one's name, what `self` asked, and `max`'s.
    pub fn clamped(&self, max: &Limits) -> (Limits, Vec<(String, u64, u64)>) {
        let (mut v, max) = (serde_json::json!(self), serde_json::json!(max));
        let mut lowered = vec![];
        for (k, x) in v.as_object_mut().expect("limits are a struct") {
            if let (Some(a), Some(b)) = (x.as_u64(), max[k].as_u64()) && a > b {
                lowered.push((k.clone(), a, b));
                *x = b.into();
            }
        }
        (serde_json::from_value(v).expect("same shape"), lowered)
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
        // With leases, a write is answered once its version counts in the
        // store, unless the config says otherwise (see `CaptureConfig`).
        let leased = !cfg.leases.is_empty();
        let said = toml::from_str::<toml::Table>(&text)?.get("capture").is_some_and(|c| c.get("remote_ack").is_some());
        if let Some(capture) = cfg.capture.as_mut().filter(|_| leased && !said) {
            capture.remote_ack = true;
        }
        // Every node must see the same leases and heads, written
        // conditionally: memory and directories are each node's own, and
        // `object_store`'s local files can't update a record conditionally.
        // (A programmatic config's caller brings stores its nodes share, as
        // tests do.)
        let store = cfg.capture.as_ref().and_then(|c| c.store.as_deref()).unwrap_or_default();
        for (key, url) in [("leases", cfg.leases.as_str()), ("capture.store", store)].iter().filter(|_| leased) {
            if ["memory:", "file:", "/"].iter().any(|own| url.starts_with(own)) {
                bail!("with leases, {key} is an object store every node shares, such as s3://bucket/prefix, not {url:?}");
            }
        }
        // A relative data_dir or secrets is relative to the config file
        // (joining keeps an absolute one as it is).
        let dir = path.parent().unwrap_or(Path::new("."));
        cfg.data_dir = dir.join(&cfg.data_dir);
        cfg.secrets = cfg.secrets.map(|s| dir.join(s));
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.leases.is_empty() && self.name.is_empty() {
            bail!("a node with leases has a name");
        }
        if self.lease_ms == 0 {
            bail!("lease_ms is 0: a node renews its leases every lease_ms / 4");
        }
        if self.capture.as_ref().is_some_and(|c| c.remote_ack && c.store.is_none()) {
            bail!("capture.remote_ack needs a capture.store");
        }
        if !self.leases.is_empty() && self.capture.as_ref().is_none_or(|c| c.store.is_none()) {
            bail!("a node with leases ships its objects' versions (capture.store): another node restores an object it takes over from there");
        }
        crate::egress::EgressPolicy::parse(&[], &self.limits.egress, self.dev).context("limits.egress")?;
        crate::gate::trusted(&self.clients.trusted)?;
        dev_on_loopback(self.dev, &self.listen, cfg!(feature = "dev"))
    }
}

impl ProxyConfig {
    /// What `nanovm proxy --config` reads.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: ProxyConfig = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.nodes.is_empty() {
            bail!("the proxy has no nodes");
        }
        // Else a node that doesn't accept a connection in time would be taken
        // for one that took the request and may have run it (504).
        if self.failover_ms <= crate::proxy::CONNECT.as_millis() as u64 {
            bail!("failover_ms must be more than the proxy's connect timeout, {:?}", crate::proxy::CONNECT);
        }
        for h in &self.hosts {
            let name = h.strip_prefix("*.").unwrap_or(h);
            ensure!(!name.is_empty() && !name.contains(['*', ':', '/']) && *h == h.to_ascii_lowercase(), "hosts entry {h:?} is not `name` or `*.name` in lower case");
        }
        for s in &self.signers {
            let star = s.find('*').is_none_or(|i| i == s.len() - 1);
            let words: Vec<_> = s.split_whitespace().collect();
            ensure!(words.len() == 2 && words.join(" ") == *s && star && !s.contains(','), "signers entry {s:?} is not `<issuer> <subject>`, the subject maybe ending in `*`");
        }
        crate::gate::trusted(&self.clients.trusted)?;
        dev_on_loopback(self.dev, &self.listen, cfg!(feature = "dev"))
    }
}

/// `listen` is an address; dev mode needs a build with the `dev` feature
/// (`built`) and a loopback address.
fn dev_on_loopback(dev: bool, listen: &str, built: bool) -> anyhow::Result<()> {
    let addr: std::net::SocketAddr = listen.parse().with_context(|| format!("listen = {listen:?}"))?;
    if dev && !built {
        bail!("dev mode needs a build with `--features dev`");
    }
    if dev && !addr.ip().is_loopback() {
        bail!("dev mode needs a loopback listen address, not {addr}");
    }
    Ok(())
}

/// The host's memory, in MiB.
fn host_memory_mb() -> u64 {
    // SAFETY: sysconf only reads.
    let (pages, size) = unsafe { (libc::sysconf(libc::_SC_PHYS_PAGES), libc::sysconf(libc::_SC_PAGESIZE)) };
    (pages.max(0) as u64 * size.max(0) as u64) >> 20
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

    /// Clamping caps every numeric limit at the maximum's, keeps the ones
    /// already under it, and leaves the egress list the app's own.
    #[test]
    fn clamped_caps_each_number_and_keeps_egress() {
        let mine = Limits {
            memory_mb: 1 << 20,
            cpu_ms: 1,
            max_queue: usize::MAX,
            max_resources: 0,
            egress: vec!["example.com".into()],
            ..Limits::default()
        };
        let max = Limits {
            memory_mb: 32,
            cpu_ms: 100,
            max_queue: 8,
            egress: vec!["*.other".into()],
            ..Limits::default()
        };
        let (c, lowered) = mine.clamped(&max);
        let mut names: Vec<_> = lowered.iter().map(|(k, ..)| k.as_str()).collect();
        names.sort();
        assert_eq!(names, ["max_queue", "memory_mb"], "{lowered:?}");
        assert_eq!(
            (c.memory_mb, c.cpu_ms, c.max_queue, c.max_resources),
            (32, 1, 8, 0)
        );
        assert_eq!(c.egress, ["example.com"]);
        assert_eq!(c.request_timeout_ms, Limits::default().request_timeout_ms);
        let d = Limits::default();
        assert_eq!(serde_json::json!(d.clamped(&d).0), serde_json::json!(d));
    }

    /// Validation refuses a listen address that isn't one, dev mode off
    /// loopback, and an egress cap naming a private address outside it.
    #[test]
    fn validate_checks_listen_and_dev() {
        let mut cfg = NodeConfig::default();
        cfg.validate().unwrap();
        cfg.listen = "nowhere".into();
        assert!(cfg.validate().unwrap_err().to_string().contains("listen"));
        cfg.dev = true;
        for ok in ["127.0.0.1:1", "[::1]:1"] {
            cfg.listen = ok.into();
            cfg.validate().unwrap();
        }
        for public in ["0.0.0.0:1", "10.0.0.1:1", "[::]:1"] {
            cfg.listen = public.into();
            assert!(cfg.validate().unwrap_err().to_string().contains("dev mode"), "{public}");
        }
        cfg.limits.egress = vec!["10.0.0.1".into()];
        cfg.listen = "127.0.0.1:1".into();
        cfg.validate().unwrap();
        cfg.dev = false;
        assert!(cfg.validate().unwrap_err().to_string().contains("limits.egress"));
        cfg.limits.egress.clear();
        cfg.validate().unwrap();
        let proxy = ProxyConfig { dev: true, ..ProxyConfig::default() };
        assert!(proxy.validate().unwrap_err().to_string().contains("dev mode"), "the default listen is public");
        // A build without the feature refuses dev mode, even on loopback.
        let e = dev_on_loopback(true, "127.0.0.1:1", false).unwrap_err().to_string();
        assert!(e.contains("--features dev"), "{e}");
        dev_on_loopback(false, "127.0.0.1:1", false).unwrap();
    }

    /// An empty file is the documented defaults; a partial table keeps the
    /// defaults of the keys it leaves out.
    #[test]
    fn toml_defaults() {
        let cfg: NodeConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.listen, "127.0.0.1:8080");
        assert_eq!(
            (cfg.max_live_objects, cfg.memory_reservation_mb, cfg.heavy_lanes, cfg.max_component_mb),
            (1000, 128, 0, 64)
        );
        assert!(cfg.cgroup == "auto" && !cfg.dev);
        assert_eq!(ProxyConfig::default().max_sockets_per_app, 1000);

        let cfg: NodeConfig = toml::from_str("[limits]\ncpu_ms = 9\n").unwrap();
        assert_eq!((cfg.limits.cpu_ms, cfg.limits.memory_mb), (9, 64));
    }

    /// The proxy serves no host and lets no identity deploy until its
    /// operator lists them; its lists hold only what they can match: names
    /// or `*.name`, in lower case, and `<issuer> <subject>` pairs whose only
    /// `*` ends the subject.
    #[test]
    fn proxy_lists_are_empty_by_default_and_checked() {
        let cfg: ProxyConfig = toml::from_str("").unwrap();
        assert!(cfg.hosts.is_empty() && cfg.signers.is_empty());
        let check = |hosts: &[&str], signers: &[&str]| {
            let list = |l: &[&str]| l.iter().map(|s| s.to_string()).collect();
            ProxyConfig { hosts: list(hosts), signers: list(signers), ..ProxyConfig::default() }.validate().map_err(|e| e.to_string())
        };
        check(&["a.com", "*.a.com", "localhost"], &["https://i s", "https://i repo:org/*", "https://i *"]).unwrap();
        for bad in ["", "*", "*.", "A.com", "a.com:443", "*a.com", "a.*.com", "http://a.com"] {
            assert!(check(&[bad], &[]).unwrap_err().contains("hosts entry"), "{bad:?}");
        }
        for bad in ["https://i", "https://i  s", "https://i s t", " https://i s", "https://i\ts", "https://* s", "https://i s*x", "https://i a,b"] {
            assert!(check(&[], &[bad]).unwrap_err().contains("signers entry"), "{bad:?}");
        }
    }

    /// A worker may use a quarter of the host's memory and all its cores
    /// but one (on one core, that core); 0 still leaves a limit unset.
    #[test]
    fn worker_limits_are_sized_from_the_host() {
        let w = WorkerLimits::default();
        assert!(host_memory_mb() >= 64 && w.memory_mb == host_memory_mb() / 4, "{w:?}");
        let cores = std::thread::available_parallelism().unwrap().get();
        assert_eq!(w.cpus, cores.saturating_sub(1).max(1) as f64);
        assert_eq!(w.pids, 0);
        let off: NodeConfig = toml::from_str("[worker]\nmemory_mb = 0\ncpus = 0.0").unwrap();
        assert_eq!((off.worker.memory_mb, off.worker.cpus), (0, 0.0));
    }

    /// A misspelt key anywhere is an error rather than silently a default,
    /// and apps come from registries, not the config.
    #[test]
    fn toml_refuses_unknown_keys() {
        for bad in [
            "listn = \"x\"",
            "[limits]\nmemory = 1",
            "[worker]\ncpu = 1",
            "[[app]]\nname = \"a\"",
        ] {
            assert!(toml::from_str::<NodeConfig>(bad).is_err(), "{bad}");
        }
    }

    /// A node with leases needs a store to ship to, from which another node
    /// restores its objects, and answers a write once it is shipped unless
    /// its config says otherwise; one node answers once it is durable here.
    #[test]
    fn leases_need_a_store_and_acknowledge_remotely_by_default() {
        let dir = std::env::temp_dir().join(format!("nanovm-config-ack-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("n.toml");
        let ack = |text: &str| {
            std::fs::write(&path, text).unwrap();
            NodeConfig::load(&path).map(|c| c.capture.unwrap().remote_ack).map_err(|e| e.to_string())
        };
        let leased = "leases = \"s3://b/leases\"\nname = \"10.0.0.1:8080\"\n";
        assert!(ack(leased).unwrap_err().contains("capture.store"));
        assert_eq!(ack(&format!("{leased}[capture]\nstore = \"s3://b/s\"")), Ok(true));
        assert_eq!(ack(&format!("{leased}[capture]\nstore = \"s3://b/s\"\nremote_ack = false")), Ok(false));
        assert_eq!(ack("[capture]\nstore = \"/s\""), Ok(false));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Hostile configs a node or proxy refuses: leases renewed in a busy
    /// loop; with leases, stores each node would have its own of (memory, a
    /// directory) or couldn't update conditionally (`object_store`'s local
    /// files: sessions would lapse at their first renewal); a proxy that
    /// would give up on a node before a connect to it can time out.
    #[test]
    fn configs_that_cannot_hold_leases_are_refused() {
        let dir = std::env::temp_dir().join(format!("nanovm-config-shared-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("n.toml");
        let load = |leases: &str, store: &str, more: &str| {
            let text = format!("leases = {leases:?}\nname = \"10.0.0.1:8080\"\n{more}\n[capture]\nstore = {store:?}\n");
            std::fs::write(&path, text).unwrap();
            NodeConfig::load(&path).map(drop).map_err(|e| e.to_string())
        };
        assert_eq!(load("s3://b/leases", "s3://b/versions", ""), Ok(()));
        assert!(load("s3://b/leases", "s3://b/versions", "lease_ms = 0").unwrap_err().contains("lease_ms"));
        for (leases, store) in [("memory:", "s3://b/v"), ("file:///l", "s3://b/v"), ("s3://b/l", "/v"), ("s3://b/l", "file:///v"), ("s3://b/l", "memory:")] {
            assert!(load(leases, store, "").unwrap_err().contains("every node shares"), "{leases} {store}");
        }
        let proxy = |failover_ms| ProxyConfig { failover_ms, ..Default::default() }.validate().map_err(|e| e.to_string());
        assert!(proxy(1000).unwrap_err().contains("connect timeout"));
        assert_eq!(proxy(1001).map_err(drop), Ok(()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Loading resolves a relative data or secrets directory against the
    /// config file's directory, and validates.
    #[test]
    fn load_resolves_relative_paths_and_validates() {
        let dir = std::env::temp_dir().join(format!("nanovm-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("n.toml");
        std::fs::write(&path, "").unwrap();
        let cfg = NodeConfig::load(&path).unwrap();
        assert_eq!(cfg.data_dir, dir.join("nanovm-data"));
        assert_eq!(cfg.app_dirs("a")[1], dir.join("nanovm-data/tmp/a"));
        std::fs::write(&path, "data_dir = \"/abs\"").unwrap();
        assert_eq!(NodeConfig::load(&path).unwrap().data_dir, Path::new("/abs"));
        std::fs::write(&path, "secrets = \"s\"").unwrap();
        assert_eq!(NodeConfig::load(&path).unwrap().secrets, Some(dir.join("s")));

        std::fs::write(&path, "dev = true\nlisten = \"0.0.0.0:1\"").unwrap();
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
