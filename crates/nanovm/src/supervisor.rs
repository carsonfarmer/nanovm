//! The supervisor (`nanovm serve`) runs no guest code. It serves the public
//! HTTP front end, runs one jailed worker process (`nanovm worker`) per app,
//! and makes every outbound request on the workers' behalf under their app's
//! egress policy, so the network is reachable only from outside the jails.
//!
//! An app is local (from the node config) or comes from a registry, named
//! per request by the proxy (`registry`). A request naming a digest the app's
//! worker isn't running switches the app to it: the new version is fetched
//! and verified, the old worker drains (no new requests; in-flight ones
//! finish), and a fresh worker starts, so an object is never live in two.
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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
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
use tokio::sync::{oneshot, watch};

use crate::config::{AppConfig, Limits, NodeConfig};
use crate::egress::{EgressPolicy, serve_forwarded};
use crate::jail::Jail;
use crate::link::{self, Dialer};
use crate::registry::{APP_HEADER, AppRef, MIN_VERSION_HEADER, Registry, TRUST_HEADER};
use crate::metrics::rss_bytes;
use crate::object::{RespBody, error_response};
use crate::table::{Node, valid_name};
use crate::ws::{self, Sockets};

pub(crate) type Body = UnsyncBoxBody<Bytes, hyper::Error>;

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
    registry: Registry,
    workers: Mutex<HashMap<String, Arc<Worker>>>,
    /// Held while giving a new app its uid.
    uids: Mutex<()>,
    /// Open WebSockets, across workers.
    sockets: Arc<AtomicUsize>,
    started: Instant,
}

struct Worker {
    name: String,
    /// The app's config, for a local app; a registry app's comes with each
    /// version.
    local: Option<AppConfig>,
    live: Mutex<Option<Arc<Live>>>,
    /// Held while (re)starting, so starts are single-flight; holds the time
    /// of the last failed start.
    starting: tokio::sync::Mutex<Option<Instant>>,
    starts: AtomicU64,
    /// Requests that failed between the supervisor and the worker.
    link_errors: AtomicU64,
}

/// A running worker process. Every request to it, and every response body
/// still being read, holds this; draining waits for them.
struct Live {
    link: Arc<Dialer<Body>>,
    pid: u32,
    /// The registry digest and version it runs (`None` for a local app).
    digest: Option<String>,
    version: Option<String>,
    /// How long its longest request may take, to bound a drain.
    drain: Duration,
    limits: Limits,
    sockets: Arc<Sockets>,
    /// Reads as changed once the process is gone, closing its sockets.
    gone: watch::Receiver<()>,
    /// Dropping or taking this kills the process. It reads as closed once
    /// the process is gone, because its watcher drops the receiver then.
    kill: Mutex<Option<oneshot::Sender<()>>>,
}

impl Live {
    fn alive(&self) -> bool {
        self.kill.lock().as_ref().is_some_and(|k| !k.is_closed())
    }
}

impl Worker {
    fn new(name: &str, local: Option<AppConfig>) -> Arc<Worker> {
        Arc::new(Worker {
            name: name.to_string(),
            local,
            live: Mutex::default(),
            starting: Default::default(),
            starts: AtomicU64::new(0),
            link_errors: AtomicU64::new(0),
        })
    }

    fn live(&self) -> Option<Arc<Live>> {
        self.live.lock().clone().filter(|l| l.alive())
    }
}

impl Supervisor {
    /// Starts a worker for every local app; fails if any of them fails to
    /// start. `threads` is the number of front-end threads that will call
    /// `handle`.
    pub async fn new(cfg: NodeConfig, exe: PathBuf, threads: usize) -> anyhow::Result<Arc<Supervisor>> {
        cfg.validate()?;
        let workers = cfg.apps.iter().map(|a| (a.name.clone(), Worker::new(&a.name, Some(a.clone())))).collect();
        let sup = Arc::new(Supervisor {
            registry: Registry::new(cfg.data_dir.join("registry"))?,
            cfg,
            threads,
            exe,
            workers: Mutex::new(workers),
            uids: Mutex::default(),
            sockets: Arc::default(),
            started: Instant::now(),
        });
        let local: Vec<_> = sup.workers.lock().values().cloned().collect();
        for w in local {
            sup.start(&w, None)
                .await
                .with_context(|| format!("starting the worker for app {:?}", w.name))?;
        }
        Ok(sup)
    }

    pub async fn handle(self: Arc<Self>, req: Request<Incoming>) -> Response<RespBody> {
        let mut req = req.map(|b| b.boxed_unsync());
        match req.uri().path() {
            "/_nanovm/stats" => return self.stats().await,
            "/_nanovm/events" => {
                let q: HashMap<_, _> = query(req.uri());
                let Some(w) = q.get("app").and_then(|a| self.workers.lock().get(*a).cloned()) else {
                    return error_response(404, "no-such-app", "events take ?app=<name>&since=<seq>");
                };
                let pq = format!("/_nanovm/events?since={}", q.get("since").unwrap_or(&"0"));
                let (mut parts, body) = req.into_parts();
                parts.uri = pq.parse().expect("a valid path");
                return self.forward(&w, Request::from_parts(parts, body), None).await;
            }
            _ => {}
        }
        let want = match AppRef::from_headers(req.headers()) {
            Ok(w) => w,
            Err(e) => return error_response(400, "bad-app-ref", &format!("{e:#}")),
        };
        for h in [APP_HEADER, TRUST_HEADER, MIN_VERSION_HEADER] {
            req.headers_mut().remove(h);
        }
        let app = req.uri().path().trim_start_matches('/').split('/').next().unwrap_or("");
        let w = {
            let mut workers = self.workers.lock();
            match (workers.get(app), &want) {
                (Some(w), Some(_)) if w.local.is_some() => {
                    return error_response(409, "app-is-local", "a local app can't be named from a registry");
                }
                (Some(w), _) => w.clone(),
                (None, Some(_)) if valid_name(app) && !app.starts_with('_') => {
                    workers.entry(app.to_string()).or_insert_with(|| Worker::new(app, None)).clone()
                }
                (None, _) => return error_response(404, "no-such-app", app),
            }
        };
        if ws::wants(&req) {
            return self.socket(w, req, want.as_ref()).await;
        }
        if ws::is_event(&req) {
            return error_response(400, "not-an-event", "socket events come from the node");
        }
        self.forward(&w, req, want.as_ref()).await
    }

    /// Opens a WebSocket on the object `req` names, on the worker running the
    /// version `want` names. Its events go to that worker while it is the
    /// app's, each bounded by the worker's longest request.
    async fn socket(&self, w: Arc<Worker>, req: Request<Body>, want: Option<&AppRef>) -> Response<RespBody> {
        let live = match self.live(&w, want).await {
            Ok(l) => l,
            Err(res) => return res,
        };
        let Some(object) = req.uri().path().split('/').nth(2).filter(|o| valid_name(o)).map(str::to_string) else {
            return error_response(400, "bad-object-id", "object ids are [A-Za-z0-9_.-]{1,128}");
        };
        let caps = ws::Caps {
            total: &self.sockets,
            max_total: self.cfg.max_sockets,
            per_object: live.limits.max_sockets,
            request_max: (live.limits.max_request_body_kb << 10) as usize,
            response_max: (live.limits.max_response_body_kb << 10) as usize,
            backlog: (live.limits.socket_backlog_kb << 10) as usize,
        };
        let (weak, max) = (Arc::downgrade(&live), caps.response_max);
        let deliver: ws::Deliver = Box::new(move |req| {
            let (w, weak) = (w.clone(), weak.clone());
            Box::pin(async move {
                let Some(live) = weak.upgrade().filter(|l| w.live().is_some_and(|c| Arc::ptr_eq(&c, l))) else {
                    return ws::Reply::Gone;
                };
                match live.link.send(req).await {
                    Ok(res) => ws::answer(res, max).await,
                    Err(_) => ws::Reply::Gone,
                }
            })
        });
        let (sockets, gone) = (live.sockets.clone(), live.gone.clone());
        drop(live);
        match sockets.open(&object, req, caps, gone, deliver).await {
            Ok(res) => res.map(|()| Empty::new().map_err(|e| match e {}).boxed_unsync()),
            Err((status, why)) => {
                let mut res = error_response(status, "socket-refused", why);
                res.headers_mut().insert(http::header::SEC_WEBSOCKET_VERSION, "13".parse().unwrap());
                res
            }
        }
    }

    async fn forward(&self, w: &Worker, req: Request<Body>, want: Option<&AppRef>) -> Response<RespBody> {
        let live = match self.live(w, want).await {
            Ok(l) => l,
            Err(res) => return res,
        };
        match live.link.send(req).await {
            // The body holds the worker until it has been read; see `drain`.
            Ok(res) => res.map(|b| {
                b.map_frame(move |f| {
                    let _ = &live;
                    f
                })
                .map_err(Into::into)
                .boxed_unsync()
            }),
            Err(e) => {
                w.link_errors.fetch_add(1, Relaxed);
                error_response(502, "worker-down", &e.to_string())
            }
        }
    }

    /// The worker running the version `want` names (any, if it names none),
    /// starting or switching to it as needed.
    async fn live(&self, w: &Worker, want: Option<&AppRef>) -> Result<Arc<Live>, Response<RespBody>> {
        let wanted = || {
            w.live()
                .filter(|l| want.is_none_or(|r| l.digest.as_ref() == Some(&r.digest)))
        };
        if let Some(l) = wanted() {
            return Ok(l);
        }
        let mut failed = w.starting.lock().await;
        if let Some(l) = wanted() {
            return Ok(l);
        }
        if failed.is_some_and(|t| t.elapsed() < RESTART_BACKOFF) {
            return Err(error_response(503, "worker-restarting", &w.name));
        }
        match self.start(w, want).await {
            Ok(()) => Ok(w.live().expect("just started")),
            Err(e) => {
                *failed = Some(Instant::now());
                eprintln!("nanovm: worker for app {:?}: {e:#}", w.name);
                Err(error_response(502, "worker-down", &format!("{e:#}")))
            }
        }
    }

    /// The uid app `name` runs as when the supervisor is root: the owner of
    /// its volumes if it has any, else the next free one. Ownership is the
    /// record, so a uid survives restarts and config changes.
    fn uid(&self, name: &str) -> anyhow::Result<Option<u32>> {
        use std::os::unix::fs::MetadataExt;
        if unsafe { libc::geteuid() } != 0 {
            return Ok(None);
        }
        let _one_at_a_time = self.uids.lock();
        let base = self.cfg.jail_uid_base;
        let objects = self.cfg.data_dir.join("objects");
        let owner = |p: &Path| std::fs::metadata(p).ok().map(|m| m.uid()).filter(|&u| u >= base);
        if let Some(uid) = owner(&objects.join(name)) {
            return Ok(Some(uid));
        }
        let taken = std::fs::read_dir(&objects).into_iter().flatten().flatten();
        let uid = taken.filter_map(|e| owner(&e.path())).max().map_or(base, |u| u + 1);
        std::fs::create_dir_all(objects.join(name))?;
        std::os::unix::fs::chown(objects.join(name), Some(uid), Some(uid))?;
        Ok(Some(uid))
    }

    /// Gets the app version `want` names (or the local app), retires the
    /// running worker, then spawns a new one, places it in its cgroup, hands
    /// it its config and component, and waits until it answers.
    async fn start(&self, w: &Worker, want: Option<&AppRef>) -> anyhow::Result<()> {
        let (app, wasm, digest, version) = match (want, &w.local) {
            (Some(r), _) => {
                let p = self.registry.load(r).await.with_context(|| format!("loading {}", r.reference))?;
                let app = AppConfig {
                    name: w.name.clone(),
                    component: PathBuf::new(),
                    static_dir: None,
                    env: p.spec.env,
                    config: p.spec.config,
                    limits: p.spec.limits.clamped(&self.cfg.registry_limits),
                    worker: self.cfg.registry_worker.clone(),
                };
                (app, p.wasm, Some(r.digest.clone()), Some(p.version.to_string()))
            }
            (None, Some(app)) => {
                let wasm = std::fs::read(&app.component).with_context(|| format!("reading {}", app.component.display()))?;
                (app.clone(), wasm, None, None)
            }
            (None, None) => bail!("requests for a registry app must name its version ({APP_HEADER})"),
        };
        let policy = EgressPolicy::parse(&app.limits.egress).context("egress")?;
        let old = w.live.lock().take();
        if let Some(old) = old {
            drain(old).await;
        }

        let uid = self.uid(&w.name)?;
        for d in self.cfg.app_dirs(&w.name) {
            std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
            if let Some(uid) = uid {
                chown_tree(&d, uid)?;
            }
        }

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
            place_in_cgroup(root, &app, pid)?;
        }

        // The worker's spec: the node config with only its app, and its uid.
        let drain = Duration::from_millis(app.limits.request_timeout_ms + app.limits.post_return_timeout_ms);
        let (limits, max_sockets) = (app.limits.clone(), app.worker.max_sockets);
        let node = NodeConfig {
            apps: vec![app],
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
        let app = w.name.clone();
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
        // `gone` changes when this ends.
        let (kill, killed) = oneshot::channel::<()>();
        let (gone_tx, gone) = watch::channel(());
        let app = w.name.clone();
        tokio::spawn(async move {
            let _gone = gone_tx;
            tokio::select! {
                status = child.wait() => eprintln!("nanovm: worker for app {app:?} exited: {}", status.map(|s| s.to_string()).unwrap_or_default()),
                _ = killed => {
                    let _ = child.kill().await;
                }
            }
        });

        let (policy, sockets) = (Arc::new(policy), Arc::new(Sockets::new(max_sockets)));
        let (s, max) = (sockets.clone(), (limits.max_response_body_kb << 10) as usize);
        egress.set_nonblocking(true)?;
        tokio::spawn(link::serve(tokio::net::UnixStream::from_std(egress)?, move |req| {
            let (policy, s) = (policy.clone(), s.clone());
            async move {
                if req.uri().host() != Some(ws::PUBLISH_HOST) {
                    return serve_forwarded(req, &policy).await;
                }
                let status = match req.uri().path().trim_end_matches('/') {
                    "/publish" => s.publish(req, max).await,
                    _ => 404,
                };
                verdict_ok(status)
            }
        }));

        let live = Arc::new(Live {
            link: Dialer::new(requests, self.threads),
            pid,
            digest,
            version,
            drain,
            limits,
            sockets,
            gone,
            kill: Mutex::new(Some(kill)),
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
        let all: Vec<_> = self.workers.lock().values().cloned().collect();
        for w in all {
            let name = w.name.clone();
            let live = w.live();
            workers.insert(
                name.clone(),
                json!({
                    "pid": live.as_ref().map(|l| l.pid),
                    "digest": live.as_ref().and_then(|l| l.digest.clone()),
                    "version": live.as_ref().and_then(|l| l.version.clone()),
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
            "sockets": self.sockets.load(Relaxed),
            "rss_mb": rss_bytes() as f64 / 1048576.0,
            "workers": workers,
            "apps": apps,
        }))
    }
}

/// Waits until nothing holds `old` but this (its requests have finished and
/// their bodies have been read), or its longest request has had its time,
/// then kills the process: what still holds it can't keep it running beside
/// the next version.
async fn drain(old: Arc<Live>) {
    let deadline = Instant::now() + old.drain;
    while Arc::strong_count(&old) > 1 && Instant::now() < deadline && old.alive() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    old.kill.lock().take();
}

/// A forwarded request's answer from the supervisor itself.
fn verdict_ok(status: u16) -> Response<wasmtime_wasi_http::WasiBody> {
    let mut res = Response::new(Empty::new().map_err(|e| match e {}).boxed_unsync());
    *res.status_mut() = http::StatusCode::from_u16(status).expect("a valid status");
    res.headers_mut().insert(crate::egress::VERDICT, "ok".parse().unwrap());
    res
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
