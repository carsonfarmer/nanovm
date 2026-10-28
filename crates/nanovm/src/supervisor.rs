//! The supervisor (`nanovm serve`) runs no guest code. It serves the public
//! HTTP front end, runs one jailed worker process (`nanovm worker`) per app,
//! and makes every outbound request on the workers' behalf under their app's
//! egress policy, so the network is reachable only from outside the jails.
//!
//! Each worker gets two socketpairs: on fd 3 the supervisor sends it requests
//! over HTTP/2, and on fd 4 the worker sends the supervisor its guests'
//! outbound requests. Its config and component arrive on stdin, read before
//! the worker jails itself.

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use bytes::Bytes;
use http::{Request, Response};
use http_body_util::{BodyExt, Empty, combinators::UnsyncBoxBody};
use hyper::body::Incoming;
use hyper::client::conn::http2;
use hyper_util::rt::{TokioExecutor, TokioIo};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};

use crate::config::{AppConfig, NodeConfig};
use crate::egress::{EgressPolicy, serve_forwarded};
use crate::jail::Jail;
use crate::metrics::rss_bytes;
use crate::object::{RespBody, error_response};
use crate::table::Node;

type Body = UnsyncBoxBody<Bytes, hyper::Error>;

/// A client for one end of a worker's socketpair; see `server::serve_h2`
/// for why resets are not counted.
fn h2_client() -> http2::Builder<TokioExecutor> {
    let mut b = http2::Builder::new(TokioExecutor::new());
    b.adaptive_window(true).max_local_error_reset_streams(None);
    b
}

/// What a worker reads from stdin before the component bytes.
#[derive(serde::Serialize, serde::Deserialize)]
struct WorkerSpec {
    /// The node config with only this worker's app in `apps`.
    node: NodeConfig,
    uid: Option<u32>,
}

/// A worker that failed to start is not retried for this long.
const RESTART_BACKOFF: Duration = Duration::from_secs(1);
/// Compiling a large component on first start can take a while.
const START_TIMEOUT: Duration = Duration::from_secs(120);

pub struct Supervisor {
    cfg: NodeConfig,
    /// The `nanovm` binary, run as `nanovm worker`.
    exe: PathBuf,
    workers: HashMap<String, Worker>,
    started: Instant,
}

struct Worker {
    index: u32,
    app: AppConfig,
    policy: Arc<EgressPolicy>,
    tx: Mutex<Option<http2::SendRequest<Body>>>,
    /// The running process. Locked while (re)starting, so starts are
    /// single-flight; dropping a `Child` kills it.
    child: tokio::sync::Mutex<Option<Child>>,
    last_failure: Mutex<Option<Instant>>,
    pid: AtomicU32,
    starts: AtomicU64,
}

impl Supervisor {
    /// Starts a worker for every app; fails if any of them fails to start.
    pub async fn new(cfg: NodeConfig, exe: PathBuf) -> anyhow::Result<Arc<Supervisor>> {
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
                    tx: Mutex::default(),
                    child: Default::default(),
                    last_failure: Mutex::default(),
                    pid: AtomicU32::new(0),
                    starts: AtomicU64::new(0),
                },
            );
        }
        let sup = Arc::new(Supervisor {
            cfg,
            exe,
            workers,
            started: Instant::now(),
        });
        for w in sup.workers.values() {
            let mut child = w.child.lock().await;
            sup.start(w, &mut child)
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
                return self.forward(w, req, &pq).await;
            }
            _ => {}
        }
        let app = req.uri().path().trim_start_matches('/').split('/').next().unwrap_or("");
        let Some(w) = self.workers.get(app) else {
            return error_response(404, "no-such-app", app);
        };
        let pq = req.uri().path_and_query().map_or("/", |p| p.as_str()).to_string();
        self.forward(w, req, &pq).await
    }

    async fn forward(&self, w: &Worker, req: Request<Body>, pq: &str) -> Response<RespBody> {
        let mut tx = match self.sender(w).await {
            Ok(tx) => tx,
            Err(res) => return res,
        };
        let (mut parts, body) = req.into_parts();
        let host = parts
            .headers
            .get(http::header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("localhost");
        parts.uri = match format!("http://{host}{pq}").parse() {
            Ok(u) => u,
            Err(_) => return error_response(400, "bad-uri", pq),
        };
        match tx.send_request(Request::from_parts(parts, body)).await {
            Ok(res) => res.map(|b| b.map_err(Into::into).boxed_unsync()),
            Err(e) => error_response(502, "worker-down", &e.to_string()),
        }
    }

    /// The worker's connection, restarting the worker if it has died.
    async fn sender(&self, w: &Worker) -> Result<http2::SendRequest<Body>, Response<RespBody>> {
        let live = || w.tx.lock().as_ref().filter(|tx| !tx.is_closed()).cloned();
        if let Some(tx) = live() {
            return Ok(tx);
        }
        let mut child = w.child.lock().await;
        if let Some(tx) = live() {
            return Ok(tx);
        }
        if w.last_failure.lock().is_some_and(|t| t.elapsed() < RESTART_BACKOFF) {
            return Err(error_response(503, "worker-restarting", &w.app.name));
        }
        match self.start(w, &mut child).await {
            Ok(()) => Ok(live().expect("just started")),
            Err(e) => {
                *w.last_failure.lock() = Some(Instant::now());
                eprintln!("nanovm: worker for app {:?}: {e:#}", w.app.name);
                Err(error_response(502, "worker-down", &format!("{e:#}")))
            }
        }
    }

    /// Spawns `w`'s process, places it in its cgroup, hands it its config and
    /// component, and waits until it answers.
    async fn start(&self, w: &Worker, child: &mut Option<Child>) -> anyhow::Result<()> {
        // Kills the previous process, if any.
        *child = None;
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
        cmd.arg("worker").env_clear().stdin(Stdio::piped()).kill_on_drop(true);
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
        let mut proc = cmd.spawn().with_context(|| format!("spawning {}", self.exe.display()))?;
        drop((worker_requests, worker_egress));
        let pid = proc.id().context("worker exited at once")?;
        if let Some(root) = &self.cfg.cgroup {
            place_in_cgroup(root, &w.app, pid)?;
        }

        let mut spec = serde_json::to_vec(&WorkerSpec {
            node: NodeConfig {
                apps: vec![w.app.clone()],
                ..self.cfg.clone()
            },
            uid,
        })?;
        spec.push(b'\n');
        let mut stdin = proc.stdin.take().expect("piped");
        stdin.write_all(&spec).await?;
        stdin.write_all(&wasm).await?;
        drop(stdin);

        let policy = w.policy.clone();
        egress.set_nonblocking(true)?;
        let egress = tokio::net::UnixStream::from_std(egress)?;
        tokio::spawn(crate::server::serve_h2(egress, move |req| {
            let policy = policy.clone();
            async move { serve_forwarded(req, &policy).await }
        }));

        requests.set_nonblocking(true)?;
        let requests = tokio::net::UnixStream::from_std(requests)?;
        let ready = tokio::time::timeout(START_TIMEOUT, async {
            let (mut tx, conn) = h2_client().handshake(TokioIo::new(requests)).await?;
            let app = w.app.name.clone();
            tokio::spawn(async move {
                if let Err(e) = conn.await {
                    eprintln!("nanovm: worker for app {app:?}: {e:?}");
                }
            });
            let res = tx.send_request(get("/_nanovm/stats")).await?;
            anyhow::ensure!(res.status() == 200, "worker answered {}", res.status());
            anyhow::Ok(tx)
        });
        let tx = match ready.await {
            Ok(Ok(tx)) => tx,
            Ok(Err(e)) => {
                let status = proc.wait().await.map(|s| s.to_string()).unwrap_or_default();
                bail!("worker exited ({status}): {e:#}");
            }
            Err(_) => bail!("worker did not answer within {START_TIMEOUT:?}"),
        };
        *w.tx.lock() = Some(tx);
        *child = Some(proc);
        w.pid.store(pid, Relaxed);
        w.starts.fetch_add(1, Relaxed);
        Ok(())
    }

    async fn stats(&self) -> Response<RespBody> {
        let mut workers = serde_json::Map::new();
        let mut apps = serde_json::Map::new();
        for (name, w) in &self.workers {
            let tx = w.tx.lock().as_ref().filter(|tx| !tx.is_closed()).cloned();
            workers.insert(
                name.clone(),
                json!({ "pid": w.pid.load(Relaxed), "starts": w.starts.load(Relaxed), "up": tx.is_some() }),
            );
            if let Some(v) = match tx {
                Some(mut tx) => get_json(&mut tx, "/_nanovm/stats").await,
                None => None,
            } {
                apps.insert(name.clone(), v);
            }
        }
        let v = json!({
            "uptime_ms": self.started.elapsed().as_millis() as u64,
            "rss_mb": rss_bytes() as f64 / 1048576.0,
            "workers": workers,
            "apps": apps,
        });
        Response::builder()
            .header("content-type", "application/json")
            .header("cache-control", "no-store")
            .body(http_body_util::Full::new(Bytes::from(v.to_string())).map_err(|e| match e {}).boxed_unsync())
            .unwrap()
    }
}

fn get(pq: &str) -> Request<Body> {
    Request::get(format!("http://worker{pq}"))
        .body(Empty::new().map_err(|e| match e {}).boxed_unsync())
        .unwrap()
}

async fn get_json(tx: &mut http2::SendRequest<Body>, pq: &str) -> Option<Value> {
    let res = tx.send_request(get(pq)).await.ok()?;
    let body = res.into_body().collect().await.ok()?.to_bytes();
    serde_json::from_slice(&body).ok()
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
    let spec: WorkerSpec = serde_json::from_slice(&input[..nl]).context("worker spec")?;
    let wasm = input.split_off(nl + 1);
    let cfg = spec.node;
    let app = cfg.apps.first().context("worker spec names no app")?;
    // SAFETY: the supervisor put these sockets at fds 3 and 4 and nothing
    // else in this process owns them.
    let (requests, upstream) = unsafe { (UnixStream::from_raw_fd(3), UnixStream::from_raw_fd(4)) };
    requests.set_nonblocking(true)?;
    upstream.set_nonblocking(true)?;
    // Raises the fd limit while that is still allowed.
    crate::engine::preflight(&cfg)?;

    let mut read_only: Vec<PathBuf> = app.static_dir.iter().cloned().collect();
    read_only.push("/proc/self".into());
    Jail {
        uid: spec.uid,
        read_write: cfg.app_dirs(&app.name).into(),
        read_only,
    }
    .enter()
    .context("entering the jail")?;

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let upstream = tokio::net::UnixStream::from_std(upstream)?;
        let (upstream, conn) = h2_client().handshake(TokioIo::new(upstream)).await?;
        let node = Node::from_wasm(cfg, vec![wasm], Some(upstream))?;
        let requests = tokio::net::UnixStream::from_std(requests)?;
        let serve = crate::server::serve_h2(requests, move |req| crate::server::handle(node.clone(), req));
        // Without its egress connection the worker can't do its job; exiting
        // lets the supervisor start a fresh one.
        tokio::select! {
            r = serve => r,
            r = conn => r.context("egress connection to the supervisor"),
        }
    })
}
