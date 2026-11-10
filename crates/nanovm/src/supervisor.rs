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
//! supervisor connections for its guests' outbound requests. Its spec
//! arrives on stdin: the node's config, which it builds its engine from (see
//! [`Spawned`]), then its app and the file in the registry's cache that holds
//! its component, which it opens before it jails itself.

use std::collections::{HashMap, HashSet};
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
use http_body_util::{BodyExt, Empty, Full, combinators::UnsyncBoxBody};
use hyper::body::Incoming;
use parking_lot::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{OwnedSemaphorePermit, watch};

use crate::config::{AppConfig, NodeConfig};
use crate::egress::{EgressPolicy, serve_forwarded};
use crate::engine::Wasm;
use crate::jail::Jail;
use crate::link::{self, Dialer};
use crate::registry::{APP_HEADER, AppRef, HOST_HEADER, MIN_VERSION_HEADER, Policy, Registry, TRUST_HEADER};
use crate::metrics::rss_bytes;
use crate::object::{RespBody, error_response};
use crate::table::{BAD_OBJECT, Node, route};
use crate::ws;

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
/// Policies remembered as accepting a running version; past this they are
/// forgotten (and checked again as they come), so requests naming ever-new
/// ones can't grow the set.
const MAX_POLICIES: usize = 16;

pub struct Supervisor {
    cfg: NodeConfig,
    runtime: tokio::runtime::Handle,
    /// Front-end threads, each with its own pool of connections per worker;
    /// the control runtime has the pool after theirs.
    threads: usize,
    /// The `nanovm` binary, run as `nanovm worker`.
    exe: PathBuf,
    registry: Registry,
    workers: Mutex<HashMap<String, Arc<Worker>>>,
    /// One per running worker, `max_apps` in all.
    slots: Arc<tokio::sync::Semaphore>,
    fetches: tokio::sync::Semaphore,
    /// Links all workers may hold open at once: each costs the supervisor a
    /// file descriptor, and its upstream another.
    links: Arc<tokio::sync::Semaphore>,
    /// Held while giving a new app its uid.
    uids: Arc<Mutex<()>>,
    /// To the proxy's private endpoint: publishes, and calls to objects.
    proxy: reqwest::Client,
    leases: Option<Arc<crate::lease::Leases>>,
    /// A worker spawned ahead of need: see [`Spawned`].
    spare: tokio::sync::Mutex<Option<Spawned>>,
    relay: Arc<crate::telemetry::Relay>,
    telemetry: Arc<crate::telemetry::Telemetry>,
    started: Instant,
}

#[derive(Default)]
struct Worker {
    name: String,
    /// Where the app is served (see `AppRef::host`).
    host: String,
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
    /// Its slot among `max_apps`, passed on to the worker that replaces it.
    slot: Mutex<Option<OwnedSemaphorePermit>>,
    /// Sending true or dropping this kills the worker. Closure acknowledges
    /// actual process exit, not just receipt of the kill request.
    kill: watch::Sender<bool>,
}

impl Live {
    fn alive(&self) -> bool {
        !*self.kill.borrow() && !self.kill.is_closed()
    }
}

impl Worker {
    fn live(&self) -> Option<Arc<Live>> {
        self.live.lock().clone().filter(|l| l.alive())
    }
}

impl Supervisor {
    /// Call within the control/egress runtime. `threads` counts front-end
    /// threads; `leases` is the store `cfg.leases` names (see `lease`).
    pub fn new(cfg: NodeConfig, exe: PathBuf, threads: usize, leases: Option<Arc<dyn object_store::ObjectStore>>) -> anyhow::Result<Arc<Supervisor>> {
        cfg.validate()?;
        anyhow::ensure!(leases.is_some() != cfg.leases.is_empty(), "a node with leases has their store");
        let ttl = Duration::from_millis(cfg.lease_ms);
        // Workers inherit its limits: a node too big for them fails here, not in each worker.
        crate::engine::preflight(&cfg)?;
        let sup = Arc::new(Supervisor {
            runtime: tokio::runtime::Handle::current(),
            registry: Registry::new(cfg.data_dir.join("registry"), cfg.max_component_mb << 20, cfg.dev)?,
            threads,
            exe,
            workers: Mutex::default(),
            slots: Arc::new(tokio::sync::Semaphore::new(cfg.max_apps)),
            fetches: tokio::sync::Semaphore::new(MAX_FETCHES),
            // A quarter of the fds: half for links and their upstreams, half
            // for clients and the rest.
            links: Arc::new(tokio::sync::Semaphore::new((crate::engine::raise_nofile()? / 4) as usize)),
            uids: Arc::default(),
            relay: Arc::new(crate::telemetry::Relay::new(&cfg.telemetry_endpoint)?),
            telemetry: Arc::new(crate::telemetry::Telemetry::new(&cfg.telemetry_endpoint, "_supervisor", env!("CARGO_PKG_VERSION"), None)?),
            leases: leases.map(|store| Arc::new(crate::lease::Leases::new(store, &cfg.name, ttl))),
            cfg,
            proxy: reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).build()?,
            spare: Default::default(),
            started: Instant::now(),
        });
        sup.clone().refill();
        // Renews the node's session, and tells each worker it holds.
        // Jailed workers can't reach a store: their versions ship from here.
        if let Some(store) = sup.cfg.capture.as_ref().map(crate::ship::store).transpose()?.flatten() {
            tokio::spawn(crate::ship::run(sup.cfg.data_dir.join("objects"), store, sup.leases.clone()));
        }
        if let Some(leases) = sup.leases.clone() {
            let weak = Arc::downgrade(&sup);
            tokio::spawn(async move {
                loop {
                    let (id, until) = leases.renew().await;
                    let Some(sup) = weak.upgrade() else { return };
                    for l in sup.workers.lock().values().filter_map(|w| w.live()) {
                        let mut req = get("/_nanovm/lease");
                        req.headers_mut().insert(crate::lease::SESSION, format!("{id} {until}").parse().unwrap());
                        tokio::spawn(async move { l.link.send(req).await });
                    }
                    drop(sup);
                    tokio::time::sleep(ttl / 4).await;
                }
            });
        }
        let weak = Arc::downgrade(&sup);
        sup.telemetry.snapshots(move || {
            let Some(sup) = weak.upgrade() else { return };
            sup.telemetry.snapshot(vec![
                ("process.memory.usage".into(), (rss_bytes() as i64).into()),
                ("process.uptime".into(), sup.started.elapsed().as_secs_f64().into()),
            ]);
            let workers: Vec<_> = sup.workers.lock().values().cloned().collect();
            for w in workers {
                let live = w.live();
                sup.telemetry.snapshot(vec![
                    ("nanovm.worker.app".into(), w.name.clone().into()),
                    ("nanovm.worker.up".into(), live.is_some().into()),
                    ("nanovm.worker.digest".into(), live.as_ref().map_or(String::new(), |l| l.digest.clone()).into()),
                    ("nanovm.worker.version".into(), live.as_ref().map_or(String::new(), |l| l.version.to_string()).into()),
                    ("process.pid".into(), (live.as_ref().map_or(0, |l| l.pid) as i64).into()),
                    ("nanovm.worker.policies".into(), (live.as_ref().map_or(0, |l| l.policies.lock().len()) as i64).into()),
                    ("nanovm.worker.starts".into(), (w.starts.load(Relaxed) as i64).into()),
                    ("nanovm.worker.link_errors".into(), (w.link_errors.load(Relaxed) as i64).into()),
                ]);
            }
        });
        Ok(sup)
    }

    /// Flush live workers and host snapshots to the collector, without an HTTP
    /// snapshot API. Readiness and ordinary requests never call this.
    pub async fn flush_telemetry(&self) -> anyhow::Result<()> {
        let live: Vec<_> = self.workers.lock().values().filter_map(|w| w.live()).collect();
        let mut failed = false;
        for l in live {
            let ok = tokio::time::timeout(Duration::from_secs(10), l.link.send(get("/_nanovm/flush")))
                .await.is_ok_and(|r| r.is_ok_and(|r| r.status() == 204));
            failed |= !ok && l.alive();
        }
        let telemetry = self.telemetry.clone();
        tokio::task::spawn_blocking(move || telemetry.flush()).await??;
        anyhow::ensure!(!failed, "a live worker failed to flush telemetry");
        Ok(())
    }

    pub async fn handle(self: Arc<Self>, req: Request<Incoming>) -> Response<RespBody> {
        // Readiness is infrastructure traffic, not a tenant request.
        if req.uri().path() == "/_nanovm/ready" && !req.headers().contains_key(APP_HEADER) {
            return self.admin(req.map(|b| b.boxed_unsync())).await;
        }
        self.telemetry.forward("supervisor.request", req, |req| self.clone().handle_request(req)).await
    }

    async fn handle_request(self: Arc<Self>, req: Request<Incoming>) -> Response<RespBody> {
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
            let host = want.host.clone();
            workers.entry(name.clone()).or_insert_with(|| Arc::new(Worker { name, host, ..Default::default() })).clone()
        };
        w.used.store(self.started.elapsed().as_millis() as u64, Relaxed);
        let live = match self.live(&w, &want).await {
            Ok(l) => l,
            Err(res) => return res,
        };
        forward(&w, live, req).await
    }

    /// The node's own routes, for requests that name no app.
    async fn admin(&self, req: Request<Body>) -> Response<RespBody> {
        match req.uri().path() {
            "/_nanovm/ready" => Response::builder().status(204).body(Empty::new().map_err(|e| match e {}).boxed_unsync()).unwrap(),
            path if path.starts_with("/_nanovm/") => error_response(404, "not-found", path),
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
        let ensured = async move {
            let live = sup.ensure(&w, &want).await;
            sup.refill();
            live
        };
        match self.runtime.spawn(ensured).await {
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
                let mut accepted = l.policies.lock();
                if accepted.len() >= MAX_POLICIES {
                    accepted.clear();
                }
                accepted.insert(want.policy.clone());
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
                (Arc::strong_count(&l) == 2).then(|| (o.used.load(Relaxed), o.clone()))
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

    /// Gets the app version `want` names, retires the running worker, then
    /// takes the spare worker (or spawns one), places it in its cgroup,
    /// hands it its app and component, and waits until it answers.
    async fn start(&self, w: &Worker, want: &AppRef) -> anyhow::Result<Arc<Live>> {
        let p = self.fetch(self.registry.load(want)).await.with_context(|| format!("loading {}", want.reference))?;
        let app = AppConfig {
            name: w.name.clone(),
            digest: want.digest.clone(),
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

        // Walking directories blocks; this thread serves other requests.
        let (cfg, name, uids) = (self.cfg.clone(), w.name.clone(), self.uids.clone());
        let uid = tokio::task::spawn_blocking(move || app_dirs(&cfg, &name, &uids)).await??;

        let mut spare = self.spare.lock().await.take();
        // One that has died meanwhile is replaced.
        let Spawned { mut child, mut stdin, requests, egress } = match spare.take_if(|s| s.child.try_wait().is_ok_and(|s| s.is_none())) {
            Some(s) => s,
            None => self.spawn().await?,
        };
        let pid = child.id().context("worker exited at once")?;
        if let Some(root) = &self.cfg.cgroup {
            place_in_cgroup(root, &app, pid)?;
        }

        // The rest of the worker's spec: its app, its uid, its component and our pid.
        let drain = Duration::from_millis(app.limits.request_timeout_ms + app.limits.post_return_timeout_ms);
        // As many links to the supervisor as its objects may use at once:
        // each its outbound requests and a publish.
        let links = self.cfg.max_live_objects * (app.limits.max_outbound_concurrency + 1);
        let mut spec = serde_json::to_vec(&(&app, uid, &p.component, std::process::id()))?;
        spec.push(b'\n');
        stdin.write_all(&spec).await?;
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

        // Keep the receiver until the child is reaped: sender closure is
        // the drain barrier even when killing takes time.
        let (kill, mut killed) = watch::channel(false);
        let app = w.name.clone();
        tokio::spawn(async move {
            tokio::select! {
                status = child.wait() => eprintln!("nanovm: worker for app {app:?} exited: {}", status.map(|s| s.to_string()).unwrap_or_default()),
                _ = killed.changed() => {
                    let _ = child.kill().await;
                }
            }
        });

        let policy = Arc::new(policy);
        let (http, proxy, app, host) = (self.proxy.clone(), self.cfg.proxy.clone(), w.name.clone(), w.host.clone());
        let leases = self.leases.clone();
        let (telemetry, digest) = (self.relay.clone(), want.digest.clone());
        let telemetry_slots = Arc::new(tokio::sync::Semaphore::new(3));
        egress.set_nonblocking(true)?;
        let link_caps = vec![Arc::new(tokio::sync::Semaphore::new(links)), self.links.clone()];
        tokio::spawn(link::serve(tokio::net::UnixStream::from_std(egress)?, link_caps, move |req| {
            let (policy, http, proxy, app, host) = (policy.clone(), http.clone(), proxy.clone(), app.clone(), host.clone());
            let (telemetry, digest, slots, leases) = (telemetry.clone(), digest.clone(), telemetry_slots.clone(), leases.clone());
            async move {
                if req.uri().host() != Some(ws::PUBLISH_HOST) {
                    return serve_forwarded(req, &policy).await;
                }
                if let Some(leases) = &leases {
                    if req.uri().path().starts_with(crate::lease::PATH) {
                        return crate::lease::serve(leases, &app, req).await.map(|b| Full::new(b).map_err(|e| match e {}).boxed_unsync());
                    }
                    if req.uri().path().starts_with(crate::egress::CALLS) {
                        return call(&http, &proxy, &app, &host, req).await;
                    }
                }
                if req.uri().path().starts_with(crate::telemetry::PATH) {
                    let Ok(_slot) = slots.try_acquire() else { return verdict_ok(429) };
                    return telemetry.handle(req, &app, &digest).await
                        .map(|r| r.map(|b| Full::new(b).map_err(|e| match e {}).boxed_unsync()))
                        .unwrap_or_else(verdict_ok);
                }
                if req.method() != http::Method::POST || req.uri().path().trim_end_matches('/') != "/publish" {
                    return verdict_ok(404);
                }
                let object = req.headers().get(ws::OBJECT_HEADER).cloned().unwrap_or_else(|| http::HeaderValue::from_static(""));
                let status = http.post(format!("{proxy}/publish")).header(APP_HEADER, app).header(ws::OBJECT_HEADER, object)
                    .timeout(Duration::from_secs(30)).body(reqwest::Body::wrap(req.into_body())).send().await
                    .map_or(502, |r| r.status().as_u16());
                verdict_ok(status)
            }
        }));

        let live = Arc::new(Live {
            link: Dialer::new(requests, self.threads + 1),
            pid,
            digest: want.digest.clone(),
            version: p.version,
            policies: Mutex::new(HashSet::from([want.policy.clone()])),
            drain,
            slot: Mutex::new(Some(slot)),
            kill,
        });
        let ready = tokio::time::timeout(START_TIMEOUT, async {
            let res = live.link.send(get("/_nanovm/ready")).await?;
            anyhow::ensure!(res.status() == 204, "worker answered {}", res.status());
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

    /// Spawns a worker and gives it the node's config, from which it builds
    /// its engine; it then waits for its app (see `worker_main`).
    async fn spawn(&self) -> anyhow::Result<Spawned> {
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
        let mut stdin = child.stdin.take().expect("piped");
        let mut spec = serde_json::to_vec(&self.cfg)?;
        spec.push(b'\n');
        stdin.write_all(&spec).await?;
        Ok(Spawned { child, stdin, requests, egress })
    }

    /// Spawns the next worker ahead of need, unless one is waiting.
    fn refill(self: Arc<Self>) {
        self.runtime.clone().spawn(async move {
            let mut spare = self.spare.lock().await;
            if spare.is_none() {
                match self.spawn().await {
                    Ok(s) => *spare = Some(s),
                    Err(e) => eprintln!("nanovm: spawning a worker: {e:#}"),
                }
            }
        });
    }
}

/// A worker process that has built its engine for this node (the pool
/// alone took 7 ms) and waits for its app. One is kept spawned ahead, so a
/// start skips spawning a process and building its engine. It is given
/// nothing of an app's, nor any uid, cgroup or jail, until it is taken: what
/// it holds comes from the node's config alone, which a supervisor never
/// changes. It exits when its stdin closes, so with the supervisor too.
struct Spawned {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    requests: UnixStream,
    egress: UnixStream,
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
/// then kills the process and waits for it to be gone: what still holds it
/// can't keep it running beside the next version.
async fn drain(old: Arc<Live>) {
    let deadline = Instant::now() + old.drain;
    while Arc::strong_count(&old) > 1 && Instant::now() < deadline && old.alive() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    old.kill.send_replace(true);
    old.kill.closed().await;
}

/// Sends a worker's call to another object of its app on to the proxy, which
/// finds the object's node; the supervisor names the app.
async fn call(http: &reqwest::Client, proxy: &str, app: &str, host: &str, req: Request<Incoming>) -> Response<wasmtime_wasi_http::WasiBody> {
    let url = format!("{proxy}{}", req.uri().path_and_query().map_or("", |p| p.as_str()));
    let (mut parts, body) = req.into_parts();
    parts.headers.remove(http::header::HOST);
    for (k, v) in [(APP_HEADER, app), (HOST_HEADER, host)] {
        parts.headers.insert(k, v.parse().expect("app names and hosts are header-safe"));
    }
    match http.request(parts.method, url).headers(parts.headers).body(reqwest::Body::wrap(body)).send().await {
        Ok(res) => {
            let mut res = Response::from(res).map(|b| b.map_err(|_| wasmtime_wasi_http::Error::ConnectionTerminated).boxed_unsync());
            res.headers_mut().insert(crate::egress::VERDICT, "ok".parse().unwrap());
            res
        }
        Err(_) => {
            let mut res = verdict_ok(502);
            res.headers_mut().insert(crate::egress::VERDICT, "ConnectionRefused".parse().unwrap());
            res
        }
    }
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

/// Makes app `name`'s directories, and when the supervisor is root gives
/// them the uid the app runs as (returned): the owner of its volumes if it
/// has any, else the next free one. Ownership is the record, so a uid
/// survives restarts and config changes.
fn app_dirs(cfg: &NodeConfig, name: &str, uids: &Mutex<()>) -> anyhow::Result<Option<u32>> {
    use std::os::unix::fs::MetadataExt;
    let volume = cfg.app_dirs(name)[0].clone();
    std::fs::create_dir_all(&volume)?;
    let ownership = std::fs::File::open(&volume)?;
    ownership.try_lock().context("app storage is already owned or cannot be locked")?;
    let uid = if unsafe { libc::geteuid() } != 0 {
        None
    } else {
        let _one_at_a_time = uids.lock();
        let base = cfg.jail_uid_base;
        let objects = cfg.data_dir.join("objects");
        let owner = |p: &Path| std::fs::metadata(p).ok().map(|m| m.uid()).filter(|&u| u >= base);
        let uid = owner(&objects.join(name)).unwrap_or_else(|| {
            let taken = std::fs::read_dir(&objects).into_iter().flatten().flatten();
            taken.filter_map(|e| owner(&e.path())).max().map_or(base, |u| u + 1)
        });
        std::os::unix::fs::chown(&volume, Some(uid), Some(uid))?;
        Some(uid)
    };
    for d in cfg.app_dirs(name) {
        std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
        if let Some(uid) = uid {
            chown_tree(&d, uid)?;
        }
        crate::journal::sync_ancestors(&d)?;
    }
    Ok(uid)
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
    use std::io::BufRead;
    let mut input = std::io::stdin().lock();
    let mut line = Vec::new();
    input.read_until(b'\n', &mut line)?;
    let cfg: NodeConfig = serde_json::from_slice(&line).context("worker spec: the node")?;
    // Before the jail, which stops raising the fd limit and reading sysctls.
    crate::engine::preflight(&cfg)?;
    // Before the app, which a spare worker waits for here. Neither starts
    // a thread, so the jail is still entered with one.
    let engine = crate::engine::build(&cfg)?;
    let linker = crate::app::linker(&engine)?;
    line.clear();
    input.read_until(b'\n', &mut line)?;
    let (app, uid, (digest, path), parent): (AppConfig, Option<u32>, (String, PathBuf), u32) =
        serde_json::from_slice(&line).context("worker spec: the app")?;
    // The registry's cache is outside the jail.
    let wasm = std::fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?;
    // SAFETY: the supervisor put these sockets at fds 3 and 4 and nothing
    // else in this process owns them.
    let (requests, upstream) = unsafe { (UnixStream::from_raw_fd(3), UnixStream::from_raw_fd(4)) };
    requests.set_nonblocking(true)?;

    Jail {
        uid,
        parent,
        read_write: cfg.app_dirs(&app.name).into(),
        read_only: vec!["/proc/self".into()],
    }
    .enter()
    .context("entering the jail")?;

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        // Pool 0 for the runtime's threads, 1 for the telemetry export thread.
        let upstream = Dialer::new(upstream, 2);
        let node = Node::with_engine(engine, &linker, cfg, &app, Wasm::File(wasm, &digest), Some(upstream))?;
        let requests = tokio::net::UnixStream::from_std(requests)?;
        link::serve(requests, vec![], move |req| crate::server::handle(node.clone(), req)).await
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preparation_cannot_change_an_owned_volume() {
        use std::os::unix::fs::MetadataExt;
        let cfg = NodeConfig {
            data_dir: std::env::temp_dir().join(format!("nanovm-owned-preparation-{}", std::process::id())),
            ..Default::default()
        };
        let [objects, tmp, cache] = cfg.app_dirs("app");
        std::fs::create_dir_all(&objects).unwrap();
        let ownership = std::fs::File::open(&objects).unwrap();
        ownership.try_lock().unwrap();
        let before = ownership.metadata().unwrap();
        let result = app_dirs(&cfg, "app", &Mutex::new(()));
        let after = ownership.metadata().unwrap();
        let untouched = !tmp.exists() && !cache.exists() && (before.uid(), before.gid()) == (after.uid(), after.gid());
        drop(ownership);
        std::fs::remove_dir_all(&cfg.data_dir).unwrap();
        assert!(result.is_err(), "preparation must reject an admitted owner");
        assert!(untouched, "rejected preparation changed directories or ownership");
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
