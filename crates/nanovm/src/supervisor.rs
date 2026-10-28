//! The supervisor (`nanovm serve`) runs no guest code. It serves the public
//! HTTP front end, runs one jailed worker process (`nanovm worker`) per app,
//! and makes every outbound request on the workers' behalf under their app's
//! egress policy, so the network is reachable only from outside the jails.
//!
//! Each worker gets two control sockets (see `link`): on fd 3 the supervisor
//! hands it connections for requests, and on fd 4 the worker hands the
//! supervisor connections for its guests' outbound requests. Its config and
//! component arrive on stdin, read before the worker jails itself.

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use bytes::Bytes;
use http::{Request, Response};
use http_body_util::{BodyExt, Empty, combinators::UnsyncBoxBody};
use hyper::body::Incoming;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::oneshot;

use crate::config::{AppConfig, NodeConfig};
use crate::egress::{EgressPolicy, serve_forwarded};
use crate::jail::Jail;
use crate::link::{self, Dialer};
use crate::metrics::rss_bytes;
use crate::object::{RespBody, error_response};
use crate::table::Node;

type Body = UnsyncBoxBody<Bytes, hyper::Error>;

/// A worker that failed to start is not retried for this long.
const RESTART_BACKOFF: Duration = Duration::from_secs(1);
/// Compiling a large component on first start can take a while.
const START_TIMEOUT: Duration = Duration::from_secs(120);

pub struct Supervisor {
    cfg: NodeConfig,
    /// Front-end threads, each with its own pool of connections per worker.
    threads: usize,
    /// The `nanovm` binary, run as `nanovm worker`.
    exe: PathBuf,
    workers: HashMap<String, Worker>,
    started: Instant,
}

struct Worker {
    index: u32,
    app: AppConfig,
    policy: Arc<EgressPolicy>,
    live: Mutex<Option<Arc<Live>>>,
    /// Held while (re)starting, so starts are single-flight; holds the time
    /// of the last failed start.
    starting: tokio::sync::Mutex<Option<Instant>>,
    starts: AtomicU64,
    /// Requests that failed between the supervisor and the worker.
    link_errors: AtomicU64,
}

/// A running worker process.
struct Live {
    link: Arc<Dialer<Body>>,
    pid: u32,
    /// Dropping this kills the process. It reads as closed once the process
    /// is gone, because its watcher drops the receiver then.
    kill: oneshot::Sender<()>,
}

impl Worker {
    fn live(&self) -> Option<Arc<Live>> {
        self.live.lock().clone().filter(|l| !l.kill.is_closed())
    }
}

impl Supervisor {
    /// Starts a worker for every app; fails if any of them fails to start.
    /// `threads` is the number of front-end threads that will call `handle`.
    pub async fn new(cfg: NodeConfig, exe: PathBuf, threads: usize) -> anyhow::Result<Arc<Supervisor>> {
        cfg.validate()?;
        let mut workers = HashMap::new();
        for (index, app) in cfg.apps.iter().enumerate() {
            let policy = EgressPolicy::parse(&app.limits.egress).with_context(|| format!("app {:?} egress", app.name))?;
            workers.insert(
                app.name.clone(),
                Worker {
                    index: index as u32,
                    app: app.clone(),
                    policy: Arc::new(policy),
                    live: Mutex::default(),
                    starting: Default::default(),
                    starts: AtomicU64::new(0),
                    link_errors: AtomicU64::new(0),
                },
            );
        }
        let sup = Arc::new(Supervisor {
            cfg,
            threads,
            exe,
            workers,
            started: Instant::now(),
        });
        for w in sup.workers.values() {
            sup.start(w)
                .await
                .with_context(|| format!("starting the worker for app {:?}", w.app.name))?;
        }
        Ok(sup)
    }

    pub async fn handle(self: Arc<Self>, req: Request<Incoming>) -> Response<RespBody> {
        let req = req.map(|b| b.boxed_unsync());
        match req.uri().path() {
            "/_nanovm/stats" => return self.stats().await,
            "/_nanovm/events" => {
                let q: HashMap<_, _> = query(req.uri());
                let Some(w) = q.get("app").and_then(|a| self.workers.get(*a)) else {
                    return error_response(404, "no-such-app", "events take ?app=<name>&since=<seq>");
                };
                let pq = format!("/_nanovm/events?since={}", q.get("since").unwrap_or(&"0"));
                let (mut parts, body) = req.into_parts();
                parts.uri = pq.parse().expect("a valid path");
                return self.forward(w, Request::from_parts(parts, body)).await;
            }
            _ => {}
        }
        let app = req.uri().path().trim_start_matches('/').split('/').next().unwrap_or("");
        let Some(w) = self.workers.get(app) else {
            return error_response(404, "no-such-app", app);
        };
        self.forward(w, req).await
    }

    async fn forward(&self, w: &Worker, req: Request<Body>) -> Response<RespBody> {
        let live = match self.live(w).await {
            Ok(l) => l,
            Err(res) => return res,
        };
        match live.link.send(req).await {
            Ok(res) => res.map(|b| b.map_err(Into::into).boxed_unsync()),
            Err(e) => {
                w.link_errors.fetch_add(1, Relaxed);
                error_response(502, "worker-down", &e.to_string())
            }
        }
    }

    /// The running worker, restarting it if it has died.
    async fn live(&self, w: &Worker) -> Result<Arc<Live>, Response<RespBody>> {
        if let Some(l) = w.live() {
            return Ok(l);
        }
        let mut failed = w.starting.lock().await;
        if let Some(l) = w.live() {
            return Ok(l);
        }
        if failed.is_some_and(|t| t.elapsed() < RESTART_BACKOFF) {
            return Err(error_response(503, "worker-restarting", &w.app.name));
        }
        match self.start(w).await {
            Ok(()) => Ok(w.live().expect("just started")),
            Err(e) => {
                *failed = Some(Instant::now());
                eprintln!("nanovm: worker for app {:?}: {e:#}", w.app.name);
                Err(error_response(502, "worker-down", &format!("{e:#}")))
            }
        }
    }

    /// Spawns `w`'s process, places it in its cgroup, hands it its config and
    /// component, and waits until it answers.
    async fn start(&self, w: &Worker) -> anyhow::Result<()> {
        // Kills the previous process, if any.
        *w.live.lock() = None;
        let uid = (unsafe { libc::geteuid() } == 0).then(|| self.cfg.jail_uid_base + w.index);
        for d in self.cfg.app_dirs(&w.app.name) {
            std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
            if let Some(uid) = uid {
                chown_tree(&d, uid)?;
            }
        }
        // Read now; this is where a fetched and verified artifact will come from.
        let wasm = std::fs::read(&w.app.component).with_context(|| format!("reading {}", w.app.component.display()))?;

        let (requests, worker_requests) = UnixStream::pair()?;
        let (egress, worker_egress) = UnixStream::pair()?;
        let (a, b) = (worker_requests.as_raw_fd(), worker_egress.as_raw_fd());
        let mut cmd = Command::new(&self.exe);
        cmd.arg("worker")
            .env_clear()
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // SAFETY: only async-signal-safe calls between fork and exec. The
        // sockets are first moved above fd 4 so neither dup2 clobbers the
        // other, then dup2 puts them at 3 and 4 without close-on-exec.
        unsafe {
            cmd.pre_exec(move || {
                let a = libc::fcntl(a, libc::F_DUPFD_CLOEXEC, 10);
                let b = libc::fcntl(b, libc::F_DUPFD_CLOEXEC, 10);
                if a < 0 || b < 0 || libc::dup2(a, 3) < 0 || libc::dup2(b, 4) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn().with_context(|| format!("spawning {}", self.exe.display()))?;
        drop((worker_requests, worker_egress));
        let pid = child.id().context("worker exited at once")?;
        if let Some(root) = &self.cfg.cgroup {
            place_in_cgroup(root, &w.app, pid)?;
        }

        // The worker's spec: the node config with only its app, and its uid.
        let node = NodeConfig {
            apps: vec![w.app.clone()],
            ..self.cfg.clone()
        };
        let mut spec = serde_json::to_vec(&(node, uid))?;
        spec.push(b'\n');
        let mut stdin = child.stdin.take().expect("piped");
        stdin.write_all(&spec).await?;
        stdin.write_all(&wasm).await?;
        drop(stdin);

        // The worker's stderr goes to ours, tagged with its app. It closes when
        // the worker exits, and the last line explains a failed start.
        let app = w.app.name.clone();
        let mut lines = tokio::io::BufReader::new(child.stderr.take().expect("piped")).lines();
        let last_words = tokio::spawn(async move {
            let mut last = String::new();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("nanovm[{app}]: {line}");
                last = line;
            }
            last
        });

        // Watches the process; dropping `kill` (with the `Live`) kills it.
        let (kill, killed) = oneshot::channel::<()>();
        let app = w.app.name.clone();
        tokio::spawn(async move {
            tokio::select! {
                status = child.wait() => eprintln!("nanovm: worker for app {app:?} exited: {}", status.map(|s| s.to_string()).unwrap_or_default()),
                _ = killed => {
                    let _ = child.kill().await;
                }
            }
        });

        let policy = w.policy.clone();
        egress.set_nonblocking(true)?;
        tokio::spawn(link::serve(tokio::net::UnixStream::from_std(egress)?, move |req| {
            let policy = policy.clone();
            async move { serve_forwarded(req, &policy).await }
        }));

        let live = Arc::new(Live {
            link: Dialer::new(requests, self.threads),
            pid,
            kill,
        });
        let ready = tokio::time::timeout(START_TIMEOUT, async {
            let res = live.link.send(get("/_nanovm/stats")).await?;
            anyhow::ensure!(res.status() == 200, "worker answered {}", res.status());
            res.into_body().collect().await?;
            anyhow::Ok(())
        });
        let e = match ready.await {
            Ok(Ok(())) => {
                *w.live.lock() = Some(live);
                w.starts.fetch_add(1, Relaxed);
                return Ok(());
            }
            Ok(Err(e)) => format!("{e:#}"),
            Err(_) => format!("no answer within {START_TIMEOUT:?}"),
        };
        // Give an exiting worker the moment it needs to say why (dropping
        // `live` would kill it first).
        let words = tokio::time::timeout(Duration::from_secs(1), last_words).await;
        bail!("worker did not start ({e}): {}", words.ok().and_then(Result::ok).unwrap_or_default())
    }

    async fn stats(&self) -> Response<RespBody> {
        let mut workers = serde_json::Map::new();
        let mut apps = serde_json::Map::new();
        for (name, w) in &self.workers {
            let live = w.live();
            workers.insert(
                name.clone(),
                json!({
                    "pid": live.as_ref().map(|l| l.pid),
                    "starts": w.starts.load(Relaxed),
                    "up": live.is_some(),
                    "link_errors": w.link_errors.load(Relaxed),
                }),
            );
            if let Some(l) = live
                && let Ok(res) = l.link.send(get("/_nanovm/stats")).await
                && let Ok(body) = res.into_body().collect().await
                && let Ok(v) = serde_json::from_slice::<Value>(&body.to_bytes())
            {
                apps.insert(name.clone(), v);
            }
        }
        crate::server::ok_json(json!({
            "uptime_ms": self.started.elapsed().as_millis() as u64,
            "rss_mb": rss_bytes() as f64 / 1048576.0,
            "workers": workers,
            "apps": apps,
        }))
    }
}

fn get(pq: &str) -> Request<Body> {
    Request::get(pq)
        .header(http::header::HOST, "worker")
        .body(Empty::new().map_err(|e| match e {}).boxed_unsync())
        .unwrap()
}

fn query(uri: &http::Uri) -> HashMap<&str, &str> {
    uri.query()
        .unwrap_or("")
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .collect()
}

/// Gives `uid` a directory tree it will own inside its jail. Skipped when the
/// top is already its own, so only the first start after a change walks it.
fn chown_tree(dir: &Path, uid: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    if std::fs::symlink_metadata(dir)?.uid() == uid {
        return Ok(());
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        std::os::unix::fs::lchown(&d, Some(uid), Some(uid)).with_context(|| format!("chown {}", d.display()))?;
        if std::fs::symlink_metadata(&d)?.is_dir() {
            stack.extend(std::fs::read_dir(&d)?.map(|e| e.map(|e| e.path())).collect::<Result<Vec<_>, _>>()?);
        }
    }
    Ok(())
}

/// Moves `pid` into `<root>/<app>` with the app's worker limits.
fn place_in_cgroup(root: &Path, app: &AppConfig, pid: u32) -> anyhow::Result<()> {
    let dir = root.join(&app.name);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating cgroup {}", dir.display()))?;
    let set = |file: &str, value: String| {
        std::fs::write(dir.join(file), &value).with_context(|| format!("writing {value:?} to {}/{file}", dir.display()))
    };
    let w = &app.worker;
    if w.memory_mb > 0 {
        set("memory.max", (w.memory_mb << 20).to_string())?;
    }
    if w.cpus > 0.0 {
        set("cpu.max", format!("{} 100000", (w.cpus * 100_000.0) as u64))?;
    }
    if w.pids > 0 {
        set("pids.max", w.pids.to_string())?;
    }
    set("cgroup.procs", pid.to_string())
}

/// `nanovm worker`: reads its spec and component from stdin, jails itself,
/// then serves its app to the supervisor until the supervisor hangs up.
pub fn worker_main() -> anyhow::Result<()> {
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    let nl = input.iter().position(|&b| b == b'\n').context("no worker spec on stdin")?;
    let (cfg, uid): (NodeConfig, Option<u32>) = serde_json::from_slice(&input[..nl]).context("worker spec")?;
    let wasm = input.split_off(nl + 1);
    let app = cfg.apps.first().context("worker spec names no app")?;
    // SAFETY: the supervisor put these sockets at fds 3 and 4 and nothing
    // else in this process owns them.
    let (requests, upstream) = unsafe { (UnixStream::from_raw_fd(3), UnixStream::from_raw_fd(4)) };
    requests.set_nonblocking(true)?;
    // Before the jail, which stops raising the fd limit and reading sysctls.
    crate::engine::preflight(&cfg)?;

    let mut read_only: Vec<PathBuf> = app.static_dir.iter().cloned().collect();
    read_only.push("/proc/self".into());
    Jail {
        uid,
        read_write: cfg.app_dirs(&app.name).into(),
        read_only,
    }
    .enter()
    .context("entering the jail")?;

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let upstream = Dialer::new(upstream, 1);
        let node = Node::from_wasm(cfg, vec![wasm], Some(upstream))?;
        let requests = tokio::net::UnixStream::from_std(requests)?;
        link::serve(requests, move |req| crate::server::handle(node.clone(), req)).await
    })
}
