//! The supervisor (`nanovm serve`) runs no guest code. It serves the public
//! HTTP front end, runs one jailed worker process (`nanovm worker`) per app,
//! and makes every outbound request on the workers' behalf under their app's
//! egress policy, so the network is reachable only from outside the jails.
//!
//! Every request names its app's version, as the proxy found it (see
//! `registry`); a request without one reaches only the node's own routes.
//! A request naming a digest the app's worker isn't running switches the app
//! to it: the new version is fetched and verified, the old worker drains (no
//! new requests; in-flight ones finish), and a fresh worker starts, so an
//! object is never live in two. Public paths are `/{object}/{rest}`.
//!
//! Each worker gets two control sockets (see `link`): on fd 3 the supervisor
//! hands it connections for requests, and on fd 4 the worker hands the
//! supervisor connections for its guests' outbound requests. Its config and
//! component arrive on stdin, read before the worker jails itself.

use std::collections::{HashMap, HashSet};
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
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{OwnedSemaphorePermit, oneshot, watch};

use crate::config::{AppConfig, NodeConfig};
use crate::egress::{EgressPolicy, serve_forwarded};
use crate::jail::Jail;
use crate::link::{self, Dialer};
use crate::registry::{APP_HEADER, AppRef, HOST_HEADER, MIN_VERSION_HEADER, Policy, Registry, TRUST_HEADER};
use crate::metrics::rss_bytes;
use crate::object::{RespBody, error_response};
use crate::table::{BAD_OBJECT, Node, route};
use crate::ws::{self, Sockets};

pub(crate) type Body = UnsyncBoxBody<Bytes, hyper::Error>;

/// A worker that failed to start is not retried for this long.
const RESTART_BACKOFF: Duration = Duration::from_secs(1);
/// Compiling a large component on first start can take a while.
const START_TIMEOUT: Duration = Duration::from_secs(120);
/// The longest line of a worker's stderr passed on whole.
const MAX_LINE: u64 = 4096;
/// Registry fetches under way at once, across apps: each may hold up to
/// `max_component_mb` in memory.
const MAX_FETCHES: usize = 4;

pub struct Supervisor {
    cfg: NodeConfig,
    /// Front-end threads, each with its own pool of connections per worker.
    threads: usize,
    /// The `nanovm` binary, run as `nanovm worker`.
    exe: PathBuf,
    registry: Registry,
    workers: Mutex<HashMap<String, Arc<Worker>>>,
    /// One per running worker, `max_apps` in all.
    slots: Arc<tokio::sync::Semaphore>,
    fetches: tokio::sync::Semaphore,
    /// Held while giving a new app its uid.
    uids: Mutex<()>,
    /// Open WebSockets, across workers.
    sockets: Arc<AtomicUsize>,
    started: Instant,
}

struct Worker {
    name: String,
    live: Mutex<Option<Arc<Live>>>,
    /// Held while starting or switching, so they are single-flight; holds
    /// the last failed start, which is answered again for a while rather
    /// than retried.
    starting: tokio::sync::Mutex<Option<Failed>>,
    starts: AtomicU64,
    /// When it was last asked for, in ms since the supervisor started.
    used: AtomicU64,
    /// Requests that failed between the supervisor and the worker.
    link_errors: AtomicU64,
}

struct Failed {
    at: Instant,
    digest: String,
    policy: Policy,
    why: String,
}

/// A running worker process. Every request to it, and every response body
/// still being read, holds this; draining waits for them.
struct Live {
    link: Arc<Dialer<Body>>,
    pid: u32,
    /// The app version it runs.
    digest: String,
    version: semver::Version,
    /// The policies that have accepted it.
    policies: Mutex<HashSet<Policy>>,
    /// How long its longest request may take, to bound a drain.
    drain: Duration,
    sockets: Arc<Sockets>,
    /// Reads as changed once the process is gone, closing its sockets.
    gone: watch::Receiver<()>,
    /// Its slot among `max_apps`, passed on to the worker that replaces it.
    slot: Mutex<Option<OwnedSemaphorePermit>>,
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
    fn live(&self) -> Option<Arc<Live>> {
        self.live.lock().clone().filter(|l| l.alive())
    }
}

impl Supervisor {
    /// `threads` is the number of front-end threads that will call `handle`.
    pub fn new(cfg: NodeConfig, exe: PathBuf, threads: usize) -> anyhow::Result<Arc<Supervisor>> {
        cfg.validate()?;
        Ok(Arc::new(Supervisor {
            registry: Registry::new(cfg.data_dir.join("registry"), cfg.max_component_mb << 20, cfg.dev)?,
            threads,
            exe,
            workers: Mutex::default(),
            slots: Arc::new(tokio::sync::Semaphore::new(cfg.max_apps)),
            fetches: tokio::sync::Semaphore::new(MAX_FETCHES),
            uids: Mutex::default(),
            cfg,
            sockets: Arc::default(),
            started: Instant::now(),
        }))
    }

    pub async fn handle(self: Arc<Self>, req: Request<Incoming>) -> Response<RespBody> {
        let mut req = req.map(|b| b.boxed_unsync());
        let want = match AppRef::from_headers(req.headers()) {
            Ok(Some(w)) => w,
            Ok(None) => return self.admin(req).await,
            Err(e) => return error_response(400, "bad-app-ref", &format!("{e:#}")),
        };
        for h in [APP_HEADER, TRUST_HEADER, MIN_VERSION_HEADER, HOST_HEADER] {
            req.headers_mut().remove(h);
        }
        if route(req.uri().path()).is_none() {
            return error_response(400, "bad-object-id", BAD_OBJECT);
        }
        let name = want.name();
        let w = {
            let mut workers = self.workers.lock();
            if !workers.contains_key(&name) && workers.len() >= 2 * self.cfg.max_apps {
                // Forget apps with no worker and no start under way, and
                // their last failures.
                workers.retain(|_, w| w.live.lock().is_some() || w.starting.try_lock().is_err());
            }
            workers.entry(name.clone()).or_insert_with(|| {
                Arc::new(Worker {
                    name,
                    live: Mutex::default(),
                    starting: Default::default(),
                    starts: AtomicU64::new(0),
                    used: AtomicU64::new(0),
                    link_errors: AtomicU64::new(0),
                })
            }).clone()
        };
        w.used.store(self.started.elapsed().as_millis() as u64, Relaxed);
        if ws::is_event(&req) {
            return error_response(400, "not-an-event", "socket events come from the node");
        }
        let live = match self.live(&w, &want).await {
            Ok(l) => l,
            Err(res) => return res,
        };
        if ws::wants(&req) {
            return socket(w, live, req).await;
        }
        forward(&w, live, req).await
    }

    /// The node's own routes, for requests that name no app.
    async fn admin(&self, req: Request<Body>) -> Response<RespBody> {
        match req.uri().path() {
            "/_nanovm/stats" => self.stats().await,
            "/_nanovm/events" => {
                let q: HashMap<_, _> = query(req.uri());
                let w = q.get("app").and_then(|a| self.workers.lock().get(*a).cloned());
                let Some((w, live)) = w.and_then(|w| Some((w.clone(), w.live()?))) else {
                    return error_response(404, "no-such-app", "events take ?app=<running app>&since=<seq>");
                };
                let pq = format!("/_nanovm/events?since={}", q.get("since").unwrap_or(&"0"));
                let (mut parts, body) = req.into_parts();
                parts.uri = pq.parse().expect("a valid path");
                forward(&w, live, Request::from_parts(parts, body)).await
            }
            _ => error_response(400, "no-app", "requests name their app (nanovm-app)"),
        }
    }

    /// The worker running the version `want` names, under its policy,
    /// starting or switching to it as needed.
    async fn live(self: &Arc<Self>, w: &Arc<Worker>, want: &AppRef) -> Result<Arc<Live>, Response<RespBody>> {
        if let Some(l) = w.live().filter(|l| l.digest == want.digest && l.policies.lock().contains(&want.policy)) {
            return Ok(l);
        }
        // In its own task: once begun, a switch finishes even if every
        // request waiting for it goes away.
        let (sup, w, want) = (self.clone(), w.clone(), want.clone());
        match tokio::spawn(async move { sup.ensure(&w, &want).await }).await {
            Ok(live) => live,
            Err(e) => Err(error_response(500, "worker-down", &e.to_string())),
        }
    }

    async fn ensure(&self, w: &Arc<Worker>, want: &AppRef) -> Result<Arc<Live>, Response<RespBody>> {
        let down = |why: &str| error_response(502, "worker-down", why);
        let mut failed = w.starting.lock().await;
        // A forgotten entry must not start a worker beside its successor.
        if !Arc::ptr_eq(self.workers.lock().entry(w.name.clone()).or_insert_with(|| w.clone()), w) {
            return Err(down("the app was restarting; try again"));
        }
        if let Some(l) = w.live().filter(|l| l.digest == want.digest) {
            if !l.policies.lock().contains(&want.policy) {
                let ok = self.fetch(self.registry.authorize(want, &l.version)).await;
                ok.map_err(|e| down(&format!("{e:#}")))?;
                l.policies.lock().insert(want.policy.clone());
            }
            return Ok(l);
        }
        if let Some(f) = &*failed
            && f.at.elapsed() < RESTART_BACKOFF
            && (&f.digest, &f.policy) == (&want.digest, &want.policy)
        {
            return Err(down(&f.why));
        }
        match self.start(w, want).await {
            Ok(l) => {
                *failed = None;
                Ok(l)
            }
            Err(e) => {
                let why = format!("{e:#}");
                eprintln!("nanovm: worker for app {:?}: {why}", w.name);
                let res = down(&why);
                *failed = Some(Failed {
                    at: Instant::now(),
                    digest: want.digest.clone(),
                    policy: want.policy.clone(),
                    why,
                });
                Err(res)
            }
        }
    }

    /// Runs a registry fetch: a few at a time across the node, each within
    /// `load_timeout_ms` in all (waiting its turn included).
    async fn fetch<T>(&self, f: impl Future<Output = anyhow::Result<T>>) -> anyhow::Result<T> {
        let limit = Duration::from_millis(self.cfg.load_timeout_ms);
        let turn = async {
            let _turn = self.fetches.acquire().await?;
            f.await
        };
        tokio::time::timeout(limit, turn).await.map_err(|_| anyhow::anyhow!("registry fetch took over {limit:?}"))?
    }

    /// A slot for `w`'s first worker: a free one, else that of the least
    /// recently used idle app, whose worker is retired for it.
    async fn slot(&self, w: &Worker) -> anyhow::Result<OwnedSemaphorePermit> {
        if let Ok(slot) = self.slots.clone().try_acquire_owned() {
            return Ok(slot);
        }
        let full = || anyhow::anyhow!("the node runs its most apps ({}), none idle", self.cfg.max_apps);
        let idle = self
            .workers
            .lock()
            .values()
            .filter(|o| o.name != w.name)
            .filter_map(|o| {
                let l = o.live.lock().clone()?;
                // Held only by its slot and here: no requests or bodies.
                (Arc::strong_count(&l) == 2 && l.sockets.is_empty()).then(|| (o.used.load(Relaxed), o.clone()))
            })
            .min_by_key(|(used, _)| *used)
            .map(|(_, o)| o)
            .ok_or_else(full)?;
        let Ok(_switching) = idle.starting.try_lock() else { return Err(full()) };
        let old = idle.live.lock().take().ok_or_else(full)?;
        let slot = old.slot.lock().take();
        drain(old).await;
        slot.ok_or_else(full)
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

    /// Gets the app version `want` names, retires the running worker, then
    /// spawns a new one, places it in its cgroup, hands it its config and
    /// component, and waits until it answers.
    async fn start(&self, w: &Worker, want: &AppRef) -> anyhow::Result<Arc<Live>> {
        let p = self.fetch(self.registry.load(want)).await.with_context(|| format!("loading {}", want.reference))?;
        let app = AppConfig {
            name: w.name.clone(),
            env: p.spec.env,
            config: p.spec.config,
            limits: p.spec.limits.clamped(&self.cfg.limits),
            worker: self.cfg.worker.clone(),
        };
        let policy = EgressPolicy::parse(&app.limits.egress, self.cfg.dev).context("egress")?;
        let old = w.live.lock().take();
        let slot = match old {
            Some(old) => {
                let slot = old.slot.lock().take();
                drain(old).await;
                slot
            }
            None => None,
        };
        let slot = match slot {
            Some(slot) => slot,
            None => self.slot(w).await?,
        };

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

        // The worker's spec: the node config, its app, and its uid.
        let drain = Duration::from_millis(app.limits.request_timeout_ms + app.limits.post_return_timeout_ms);
        // As many links to the supervisor as its objects may use at once:
        // each its outbound requests and a publish.
        let links = self.cfg.max_live_objects * (app.limits.max_outbound_concurrency + 1);
        let kb = |k: u64| (k << 10) as usize;
        let caps = ws::Caps {
            total: self.sockets.clone(),
            max_total: self.cfg.max_sockets,
            per_app: app.worker.max_sockets,
            per_object: app.limits.max_sockets,
            request_max: kb(app.limits.max_request_body_kb),
            response_max: kb(app.limits.max_response_body_kb),
            backlog: kb(app.limits.socket_backlog_kb),
        };
        let mut spec = serde_json::to_vec(&(&self.cfg, &app, uid))?;
        spec.push(b'\n');
        let mut stdin = child.stdin.take().expect("piped");
        stdin.write_all(&spec).await?;
        stdin.write_all(&p.wasm).await?;
        drop(stdin);

        // The worker's stderr goes to ours, tagged with its app, a line (of at
        // most `MAX_LINE`) at a time. It closes when the worker exits, and
        // the last line explains a failed start.
        let app = w.name.clone();
        let mut stderr = tokio::io::BufReader::new(child.stderr.take().expect("piped"));
        let last_words = tokio::spawn(async move {
            let (mut line, mut last) = (Vec::new(), String::new());
            loop {
                line.clear();
                match (&mut stderr).take(MAX_LINE).read_until(b'\n', &mut line).await {
                    Ok(0) | Err(_) => return last,
                    Ok(_) => last = String::from_utf8_lossy(&line).trim_end().to_string(),
                }
                eprintln!("nanovm[{app}]: {last}");
            }
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

        let (policy, sockets) = (Arc::new(policy), Arc::new(Sockets::new(caps)));
        let s = sockets.clone();
        egress.set_nonblocking(true)?;
        tokio::spawn(link::serve(tokio::net::UnixStream::from_std(egress)?, links, move |req| {
            let (policy, s) = (policy.clone(), s.clone());
            async move {
                if req.uri().host() != Some(ws::PUBLISH_HOST) {
                    return serve_forwarded(req, &policy).await;
                }
                let status = match req.uri().path().trim_end_matches('/') {
                    "/publish" => s.publish(req).await,
                    _ => 404,
                };
                verdict_ok(status)
            }
        }));

        let live = Arc::new(Live {
            link: Dialer::new(requests, self.threads),
            pid,
            digest: want.digest.clone(),
            version: p.version,
            policies: Mutex::new(HashSet::from([want.policy.clone()])),
            drain,
            sockets,
            gone,
            slot: Mutex::new(Some(slot)),
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
                *w.live.lock() = Some(live.clone());
                w.starts.fetch_add(1, Relaxed);
                return Ok(live);
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
                    "digest": live.as_ref().map(|l| l.digest.clone()),
                    "version": live.as_ref().map(|l| l.version.to_string()),
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

/// Opens a WebSocket on the object `req` names, on `live`. Its events go to
/// that worker while it is the app's, each bounded by its longest request.
async fn socket(w: Arc<Worker>, live: Arc<Live>, req: Request<Body>) -> Response<RespBody> {
    let object = route(req.uri().path()).expect("checked by `handle`").0.to_string();
    let (weak, max) = (Arc::downgrade(&live), live.sockets.caps.response_max);
    let deliver: ws::Deliver = Box::new(move |req| {
        let (w, weak) = (w.clone(), weak.clone());
        Box::pin(async move {
            let Some(live) = weak.upgrade().filter(|l| w.live().is_some_and(|c| Arc::ptr_eq(&c, l))) else {
                return ws::Reply::Gone;
            };
            match live.link.send(req).await {
                Ok(res) => ws::answer(res, max).await,
                Err(_) if w.live().is_some_and(|c| Arc::ptr_eq(&c, &live)) => ws::Reply::Bad(502),
                Err(_) => ws::Reply::Gone,
            }
        })
    });
    let (sockets, gone) = (live.sockets.clone(), live.gone.clone());
    drop(live);
    // Its own task: a client that hangs up mid-`OPEN` would otherwise
    // cancel it after the object accepted, and the object would never
    // hear the socket ended.
    let open = tokio::spawn(async move { sockets.open(&object, req, gone, deliver).await });
    match open.await.unwrap_or(Err((500, "opening the socket failed"))) {
        Ok(res) => res.map(|()| Empty::new().map_err(|e| match e {}).boxed_unsync()),
        Err((status, why)) => {
            let mut res = error_response(status, "socket-refused", why);
            res.headers_mut().insert(http::header::SEC_WEBSOCKET_VERSION, "13".parse().unwrap());
            res
        }
    }
}

async fn forward(w: &Worker, live: Arc<Live>, req: Request<Body>) -> Response<RespBody> {
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
    let (cfg, app, uid): (NodeConfig, AppConfig, Option<u32>) =
        serde_json::from_slice(&input[..nl]).context("worker spec")?;
    let wasm = input.split_off(nl + 1);
    // SAFETY: the supervisor put these sockets at fds 3 and 4 and nothing
    // else in this process owns them.
    let (requests, upstream) = unsafe { (UnixStream::from_raw_fd(3), UnixStream::from_raw_fd(4)) };
    requests.set_nonblocking(true)?;
    // Before the jail, which stops raising the fd limit and reading sysctls.
    crate::engine::preflight(&cfg)?;

    Jail {
        uid,
        read_write: cfg.app_dirs(&app.name).into(),
        read_only: vec!["/proc/self".into()],
    }
    .enter()
    .context("entering the jail")?;

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let upstream = Dialer::new(upstream, 1);
        let node = Node::new(cfg, &app, &wasm, Some(upstream))?;
        let requests = tokio::net::UnixStream::from_std(requests)?;
        link::serve(requests, usize::MAX, move |req| crate::server::handle(node.clone(), req)).await
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Query pairs are split on `&` and the first `=`, undecoded; a pair
    /// without `=` is skipped and a repeated key keeps its last value.
    #[test]
    fn query_pairs() {
        let q = |u: &str| {
            query(&u.parse().unwrap())
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<Vec<_>>()
        };
        assert!(q("/_nanovm/events").is_empty());
        assert!(q("/_nanovm/events?").is_empty());
        let mut got = q("/e?app=a&since=5&flag&x=1=2&app=b&=v&s=%20");
        got.sort();
        let want = [("", "v"), ("app", "b"), ("s", "%20"), ("since", "5"), ("x", "1=2")];
        assert_eq!(got, want.map(|(k, v)| (k.to_string(), v.to_string())));
    }

    /// The supervisor's own answer to a forwarded request carries the
    /// verdict a worker trusts.
    #[test]
    fn verdicts_mark_the_supervisors_answers() {
        let res = verdict_ok(204);
        assert_eq!(res.status(), 204);
        assert_eq!(res.headers()[crate::egress::VERDICT], "ok");
    }
}
