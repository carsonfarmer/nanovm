//! One durable object: an actor task that owns at most one live instance,
//! serves its queue one request at a time, and tears the instance down when it
//! goes idle or breaks a limit.

use std::collections::VecDeque;
use std::future::{Future, poll_fn};
use std::path::PathBuf;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering::Relaxed};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use http_body_util::{BodyExt, Full, combinators::UnsyncBoxBody};
use parking_lot::Mutex;
use opentelemetry::trace::Span as _;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use wasmtime::component::{Accessor, ResourceTable, ResourceTableError};
use wasmtime::{AsContextMut, ResourceLimiter, Store, Trap, UpdateDeadline};
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::p3::bindings::Service;
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

use crate::app::App;
use crate::egress::EgressHooks;
use crate::metrics::{Metrics, Poison, now_ns, thread_cpu_ns};
use crate::quota::QuotaGuard;
use crate::table::Node;

/// CPU a request may use before it has to share the node's heavy lanes.
const HEAVY_AFTER_NS: u64 = 10_000_000;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type RespBody = UnsyncBoxBody<Bytes, BoxError>;

pub struct Job {
    pub req: http::Request<Bytes>,
    pub reply: oneshot::Sender<http::Response<RespBody>>,
    pub enqueued: Instant,
    pub context: opentelemetry::Context,
    pub span: crate::telemetry::Span,
    /// For a call, the objects waiting on it (see `egress::CALLERS`).
    pub callers: Vec<Arc<str>>,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Starting,
    Idle,
    Handling,
    PostReturn,
    Cold,
}

impl Phase {
    pub(crate) const ALL: [Phase; 5] = [
        Phase::Starting,
        Phase::Idle,
        Phase::Handling,
        Phase::PostReturn,
        Phase::Cold,
    ];

    pub fn as_str(self) -> &'static str {
        ["starting", "idle", "handling", "post-return", "cold"][self as usize]
    }
}

/// State of an object visible outside its actor (the table, and the epoch
/// callback, which runs while the actor's future is borrowed).
#[derive(Default)]
pub struct ObjShared {
    pub app: Arc<str>,
    pub id: Arc<str>,
    phase: AtomicU8,
    /// When the current phase began; for an idle object, its last use.
    since_ns: AtomicU64,
    /// CPU charged to the current request, up to the poll in progress.
    cpu_accum_ns: AtomicU64,
    /// The polling thread's CPU clock ([`thread_cpu_ns`]) when the poll in
    /// progress, or the request if it began in that poll, started.
    poll_start_ns: AtomicU64,
    /// The heavy lane this object's request holds until its next yield.
    lane: Mutex<Option<OwnedSemaphorePermit>>,
    /// The objects its calls are waiting on, one entry per call.
    pub waiting_on: Mutex<Vec<Arc<str>>>,
    /// The objects waiting on its request, while they do.
    pub callers: Mutex<Vec<Arc<str>>>,
    /// With more than one node, its node's session, and lease on it.
    pub leases: Option<Arc<crate::lease::Session>>,
    pub lease: Mutex<Option<crate::lease::Grant>>,
    pub context: Mutex<opentelemetry::Context>,
    /// Set by the epoch callback when it makes the guest yield.
    yielding: AtomicBool,
    /// Held by a commit, which can outlive the instance it was for.
    storage: Arc<tokio::sync::Mutex<()>>,
}

impl ObjShared {
    pub fn new(app: Arc<str>, id: &str) -> Self {
        let now = now_ns();
        ObjShared {
            app,
            id: id.into(),
            phase: AtomicU8::new(Phase::Cold as u8),
            since_ns: now.into(),
            ..Default::default()
        }
    }

    /// Whether its node may serve it and commit its changes: with one node
    /// always, else while it holds the object's lease in its current session.
    pub fn holds(&self) -> bool {
        self.leases.as_ref().is_none_or(|s| self.lease.lock().as_ref().is_some_and(|g| s.holds(g)))
    }

    /// Whether it had a lease that its node no longer holds.
    pub fn lost(&self) -> bool {
        self.leases.as_ref().is_some_and(|s| self.lease.lock().as_ref().is_some_and(|g| !s.holds(g)))
    }

    /// The epoch of its node's lease on it, which its committed versions
    /// carry; an error once the node no longer holds it, so a node that lost
    /// the object can't commit (W-10). 0 with one node.
    pub fn lease_epoch(&self) -> std::io::Result<u64> {
        let Some(s) = &self.leases else { return Ok(0) };
        match &*self.lease.lock() {
            Some(g) if s.holds(g) => Ok(g.epoch),
            _ => Err(std::io::Error::other("this node no longer holds the object's lease")),
        }
    }

    pub fn phase(&self) -> (Phase, u64) {
        (
            Phase::ALL[self.phase.load(Relaxed) as usize],
            self.since_ns.load(Relaxed),
        )
    }

    pub(crate) fn set_phase(&self, p: Phase) {
        if matches!(p, Phase::Idle | Phase::Cold) {
            self.lane.lock().take();
        }
        let now = now_ns();
        self.since_ns.store(now, Relaxed);
        self.phase.store(p as u8, Relaxed);
    }

    /// Starts a fresh CPU charge for instantiating a new instance. Without
    /// it the charge left by a killed request counted against the new
    /// instance's start-up.
    fn begin_start(&self) {
        self.cpu_accum_ns.store(0, Relaxed);
        self.poll_start_ns.store(thread_cpu_ns(), Relaxed);
    }

    fn begin_request(&self) {
        self.begin_start();
        self.set_phase(Phase::Handling);
    }

    /// Drives `fut`, charging the CPU its thread uses inside each poll
    /// (where guest code and the host calls it makes run) to the current
    /// request or start-up. Time waiting to be polled, for a core or for the
    /// disk isn't CPU, so an overloaded node doesn't turn into CPU kills.
    /// Pinned by the caller: moved in, it was held twice by this future,
    /// which made an idle object's actor 7 KB bigger.
    async fn charged<F: Future>(&self, metrics: &Metrics, mut fut: Pin<&mut F>) -> F::Output {
        poll_fn(|cx| {
            let t0 = thread_cpu_ns();
            self.poll_start_ns.store(t0, Relaxed);
            let poll = fut.as_mut().poll(cx);
            // Re-read: a request that began during this poll reset the start.
            let (t1, start) = (thread_cpu_ns(), self.poll_start_ns.load(Relaxed));
            self.cpu_accum_ns.fetch_add(t1.saturating_sub(start), Relaxed);
            metrics.guest_cpu_ns.fetch_add(t1 - t0, Relaxed);
            poll
        })
        .await
    }

    /// Leaves the CPU this thread used since `t0`, a [`thread_cpu_ns`] in
    /// the poll in progress, out of the charge: the journal's undo records
    /// and commits (and captures) are the node's work, not the guest's.
    pub fn uncharge(&self, t0: u64) {
        self.poll_start_ns.fetch_add(thread_cpu_ns().saturating_sub(t0), Relaxed);
    }

    /// Holds the object's volume for a commit until it ends, whatever comes
    /// of the request it is for: one killed at its deadline leaves it
    /// running with the disk (see `run`).
    pub async fn storage(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.storage.clone().lock_owned().await
    }

    /// CPU charged to the current request, including the poll in progress.
    /// Only on the polling thread, in a poll: the epoch callback.
    fn cpu_used_ns(&self) -> u64 {
        let start = self.poll_start_ns.load(Relaxed);
        self.cpu_accum_ns.load(Relaxed) + thread_cpu_ns().saturating_sub(start)
    }
}

/// Caps the sum of all linear memories of one instance.
#[derive(Default)]
pub struct MemLimiter {
    pub(crate) max_bytes: usize,
    used: usize,
    pub denied: bool,
}

impl ResourceLimiter for MemLimiter {
    fn memory_growing(&mut self, current: usize, desired: usize, _max: Option<usize>) -> wasmtime::Result<bool> {
        let used = self.used + desired.saturating_sub(current);
        if used > self.max_bytes {
            self.denied = true;
            return Ok(false);
        }
        self.used = used;
        Ok(true)
    }

    // Table growth is capped by the pool's `table_elements`.
    fn table_growing(&mut self, _current: usize, _desired: usize, _max: Option<usize>) -> wasmtime::Result<bool> {
        Ok(true)
    }
}

/// Keeps the last few KiB a guest wrote to stdout/stderr, so a trap can be
/// reported with the panic message that preceded it.
#[derive(Clone, Default)]
pub struct LogTail {
    buf: Arc<Mutex<VecDeque<u8>>>,
    export: Option<(Arc<crate::telemetry::Telemetry>, Arc<ObjShared>, &'static str)>,
}

const LOG_TAIL: usize = 2048;

impl LogTail {
    pub fn text(&self) -> String {
        let buf: Vec<u8> = self.buf.lock().iter().copied().collect();
        String::from_utf8_lossy(&buf).trim().to_string()
    }
}

impl IsTerminal for LogTail {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for LogTail {
    fn async_stream(&self) -> Box<dyn tokio::io::AsyncWrite + Send + Sync> {
        Box::new(self.clone())
    }
}

impl tokio::io::AsyncWrite for LogTail {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, data: &[u8]) -> Poll<std::io::Result<usize>> {
        if let Some((telemetry, object, stream)) = &self.export {
            telemetry.log(&object.id, ("log.iostream", stream), data, &object.context.lock());
        }
        let mut buf = self.buf.lock();
        buf.extend(data.iter().rev().take(LOG_TAIL).rev());
        let excess = buf.len().saturating_sub(LOG_TAIL);
        buf.drain(..excess);
        Poll::Ready(Ok(data.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// The app's scratch directories, one per live instance, behind `/tmp`.
/// One an instance leaves empty is handed to the next rather than removed:
/// a mkdir and an rmdir cost more than the rest of a cold start's
/// filesystem work, and an rmdir blocks for milliseconds on a disk mounted
/// with `discard`.
pub struct Scratch {
    dir: PathBuf,
    free: Mutex<Vec<PathBuf>>,
}

/// The name of the next new scratch directory.
static NEXT: AtomicU64 = AtomicU64::new(0);

impl Scratch {
    /// Scratch under `dir`. A previous run (a worker that was killed or
    /// retired) leaves a directory for each instance it had: one left empty
    /// is handed out again, anything else is removed. Removing them all made
    /// a worker that had served 200 objects start half a second late.
    /// `dir` itself stays: a jailed worker's Landlock rule names it.
    pub fn new(dir: PathBuf) -> std::io::Result<Arc<Scratch>> {
        std::fs::create_dir_all(&dir)?;
        let mut free = Vec::new();
        for e in std::fs::read_dir(&dir)? {
            let e = e?;
            // New names start past those left.
            let n = e.file_name().to_str().and_then(|n| n.parse::<u64>().ok());
            NEXT.fetch_max(n.map_or(0, |n| n + 1), Relaxed);
            if e.file_type()?.is_dir() && empty(&e.path()) {
                free.push(e.path());
            } else {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
        Ok(Arc::new(Scratch { dir, free: Mutex::new(free) }))
    }

    pub(crate) fn take(self: &Arc<Self>) -> std::io::Result<TmpDir> {
        let path = match self.free.lock().pop() {
            Some(path) => path,
            None => {
                let path = self.dir.join(NEXT.fetch_add(1, Relaxed).to_string());
                std::fs::create_dir(&path)?;
                path
            }
        };
        Ok(TmpDir(path, self.clone()))
    }
}

/// An instance's scratch directory: back to its [`Scratch`] when the
/// instance goes away if the instance left it empty, else removed; nothing,
/// once its path is taken (moved elsewhere).
pub(crate) struct TmpDir(pub(crate) PathBuf, Arc<Scratch>);

impl Drop for TmpDir {
    fn drop(&mut self) {
        if self.0.as_os_str().is_empty() {
            return;
        }
        if empty(&self.0) {
            self.1.free.lock().push(std::mem::take(&mut self.0));
        } else {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn empty(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut d| d.next().is_none())
}

pub struct ObjState {
    wasi: WasiCtx,
    pub(crate) table: ResourceTable,
    http: WasiHttpCtx,
    hooks: EgressHooks,
    pub(crate) limiter: MemLimiter,
    shared: Arc<ObjShared>,
    cpu_budget_ns: u64,
    cpu_exceeded: bool,
    stderr: LogTail,
    pub quota: QuotaGuard,
    pub(crate) config: Arc<wasmtime_wasi_config::WasiConfigVariables>,
    /// A request failed: the instance ends, and its restart rolls back.
    reset: bool,
    _tmp: TmpDir,
}

impl WasiView for ObjState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for ObjState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.hooks,
        }
    }
}

fn new_store(node: &Arc<Node>, app: &App, shared: &Arc<ObjShared>) -> anyhow::Result<Store<ObjState>> {
    let limits = &app.cfg.limits;
    // First, so a failed request or a crash is undone before it's counted.
    // A volume made here, or by the journal, in a directory without
    // symlinks, has none; one that was already there must not be one either.
    let volume = crate::app::object_dir(&app.objects.dir, &shared.id).join(crate::journal::VOLUME);
    let capture = node.cfg.capture.as_ref();
    let journal = crate::journal::Journal::open(&volume, limits.disk_mb << 20, shared.clone(), capture, Some(&app.objects))?;
    let data_dir = journal.volume().to_path_buf();
    anyhow::ensure!(std::fs::symlink_metadata(&data_dir)?.is_dir(), "volume {} is not a directory", data_dir.display());
    let tmp = app.scratch.take()?;

    let stderr = LogTail::default();
    let mut b = WasiCtxBuilder::new();
    b.env("NANOVM_APP", &*app.name).env("NANOVM_OBJECT_ID", &*shared.id);
    for (k, v) in &app.cfg.env {
        b.env(k, v);
    }
    // Small local-disk syscalls run inline, charged to this object's CPU
    // budget. Through `spawn_blocking` they all contend on the blocking
    // pool's one mutex. Preopens capture this, so it must come first.
    b.allow_blocking_current_thread(true);
    b.preopened_dir(&data_dir, "/data", FsPerms::ReadWrite)?;
    b.preopened_dir(&tmp.0, "/tmp", FsPerms::ReadWrite)?;
    let output = |stream| LogTail {
        buf: stderr.buf.clone(), export: Some((node.telemetry.clone(), shared.clone(), stream)),
    };
    b.stdout(output("stdout")).stderr(output("stderr"));

    // A resource can hold a header map of up to `MAX_HEAD` on the host (the
    // guest's own, or a copy of one it was sent), so an instance holds no
    // more than its memory limit has room for.
    let mut table = ResourceTable::new();
    table.set_max_capacity(limits.max_resources.min((limits.memory_mb << 20) as usize / crate::server::MAX_HEAD));
    let mut http = WasiHttpCtx::new();
    http.set_field_size_limit(crate::server::MAX_HEAD);

    // `/tmp` starts empty, so only the persistent volume holds usage.
    let quota = QuotaGuard::new(journal, limits, node.metrics.clone(), shared.clone())?;

    let state = ObjState {
        wasi: b.build(),
        table,
        http,
        hooks: EgressHooks {
            route: app.egress.clone(),
            node: Arc::downgrade(node),
            sem: Arc::new(Semaphore::new(limits.max_outbound_concurrency)),
            metrics: node.metrics.clone(),
            obj: shared.clone(),
            held: Default::default(),
            publish_max: (limits.max_response_body_kb << 10) as usize,
        },
        limiter: MemLimiter { max_bytes: (limits.memory_mb << 20) as usize, ..Default::default() },
        shared: shared.clone(),
        cpu_budget_ns: limits.cpu_ms * 1_000_000,
        cpu_exceeded: false,
        stderr,
        quota,
        config: app.config.clone(),
        reset: false,
        _tmp: tmp,
    };

    let mut store = Store::new(&node.engine, state);
    store.limiter(|s| &mut s.limiter);
    let heavy = node.heavy.clone();
    store.epoch_deadline_callback(move |mut s| {
        let st = s.data_mut();
        let used = st.shared.cpu_used_ns();
        if used > st.cpu_budget_ns {
            st.cpu_exceeded = true;
            return Err(wasmtime::format_err!("cpu budget exceeded"));
        }
        // At every tick, let the other tasks on this thread run, and hand
        // back the lane so heavy requests take turns.
        st.shared.lane.lock().take();
        st.shared.yielding.store(true, Relaxed);
        if used < HEAVY_AFTER_NS {
            return Ok(UpdateDeadline::Yield(1));
        }
        let (heavy, shared) = (heavy.clone(), st.shared.clone());
        Ok(UpdateDeadline::YieldCustom(
            1,
            Box::pin(async move {
                // A free lane is taken at once, without a yield.
                let mut yielded = false;
                poll_fn(|cx| {
                    if std::mem::replace(&mut yielded, true) {
                        return Poll::Ready(());
                    }
                    cx.waker().wake_by_ref();
                    Poll::Pending
                })
                .await;
                if let Ok(p) = heavy.acquire_owned().await {
                    *shared.lane.lock() = Some(p);
                }
            }),
        ))
    });
    store.set_epoch_deadline(1);
    Ok(store)
}

enum Exit {
    /// The table dropped our sender (eviction) and the queue is drained.
    Closed,
    /// No request arrived within the idle timeout.
    Idle,
    /// A request failed; restarting rolls back what it changed.
    Reset,
    Poisoned(Poison, String),
}

/// Makes the request's changes durable, then sends what it published (the
/// changes stand whether or not a publish gets through). Only while the node
/// holds the object's lease.
async fn commit(acc: &Accessor<ObjState>) -> std::io::Result<()> {
    let (quota, held, route, node, id) = acc.with(|mut s| {
        let s = s.data_mut();
        let held = std::mem::take(&mut *s.hooks.held.lock());
        (s.quota.clone(), held, s.hooks.route.clone(), s.hooks.node.clone(), s.shared.id.clone())
    });
    // Only while the node holds the object's lease (see `Journal::commit`):
    // otherwise neither its changes nor its publishes go out.
    quota.commit().await?;
    if !held.is_empty() {
        // Boxed: inline, sending made every idle object's actor 2 KB bigger.
        Box::pin(async move {
            for body in held {
                // All a held publish is: its body, named with its object so
                // it reaches only that object's sockets.
                let req = http::Request::post(format!("http://{}/publish", crate::ws::PUBLISH_HOST));
                let req = req.header(crate::ws::OBJECT_HEADER, &*id).body(body).expect("ids are header-safe");
                match (&route, node.upgrade()) {
                    (crate::egress::Route::Supervisor(up), _) => {
                        if let Ok(res) = up.send(req.map(|b| Full::new(b).map_err(|e| match e {}).boxed_unsync())).await {
                            let _ = res.into_body().collect().await;
                        }
                    }
                    (crate::egress::Route::Direct(_), Some(node)) => {
                        node.sockets.publish(req.map(Full::new)).await;
                    }
                    _ => {}
                }
            }
        })
        .await;
    }
    Ok(())
}

/// Reply slot for the request in flight, shared between the request loop
/// (inside the store's event loop) and the supervisor (outside it), which
/// answers on the guest's behalf if the instance has to be killed.
struct Pending {
    reply: Option<oneshot::Sender<http::Response<RespBody>>>,
    abort: Arc<AtomicBool>,
    span: crate::telemetry::Span,
    phase: &'static str,
    since: Instant,
}

impl Pending {
    fn phase(&mut self, phase: &'static str) {
        crate::telemetry::duration(&mut self.span, self.phase, self.since);
        self.phase = phase;
        self.since = Instant::now();
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        crate::telemetry::duration(&mut self.span, self.phase, self.since);
    }
}

pub async fn run(node: Arc<Node>, app: Arc<App>, shared: Arc<ObjShared>, mut rx: mpsc::Receiver<Box<Job>>) {
    let _lease = scopeguard::guard((), |()| node.release(&shared));
    loop {
        let Some(mut job) = rx.recv().await else { return };
        *shared.context.lock() = job.context.clone();
        shared.set_phase(Phase::Starting);
        // Boxed: only nodes with leases wait on it.
        if node.leases.is_some()
            && let Err(response) = Box::pin(node.acquire(&shared)).await
        {
            crate::telemetry::response(&mut job.span, &response);
            let _ = job.reply.send(response);
            shared.set_phase(Phase::Cold);
            if node.try_retire(&shared, &rx) {
                return;
            }
            continue;
        }
        // A store holds a slot until it is dropped, and the pool has room for
        // every slot. An evicted object's store lives until its actor next
        // runs, so a new object waits here until that store is gone rather
        // than finding the pool full.
        let slot = node.slots.clone().acquire_owned().await.expect("slots is never closed");
        crate::telemetry::duration(&mut job.span, "nanovm.queue.duration_ms", job.enqueued);
        job.span.set_attribute(opentelemetry::KeyValue::new("nanovm.cold_start", true));
        shared.begin_start();
        let t0 = Instant::now();
        // Retirement follows the store's fields and stream resources, not
        // ObjState::drop, which runs before those fields are destroyed.
        let mut counted = scopeguard::guard(false, |counted| {
            if counted { node.metrics.stores_live.fetch_sub(1, Relaxed); }
        });
        let started = async {
            let mut store = new_store(&node, &app, &shared)?;
            *counted = true;
            let live = node.metrics.stores_live.fetch_add(1, Relaxed) + 1;
            node.metrics.stores_peak.fetch_max(live, Relaxed);
            let svc = shared
                .charged(&node.metrics, pin!(app.pre.instantiate_async(&mut store)))
                .await?;
            anyhow::Ok((store, svc))
        }
        .await;
        crate::telemetry::duration(&mut job.span, "nanovm.instantiate.duration_ms", t0);
        match started {
            Ok((store, svc)) => {
                let exit = serve(&node, &app, &shared, store, svc, job, &mut rx).await;
                // A commit its request's deadline cut short still owns the
                // volume. Recovering it for the next instance (or another
                // actor's, once this one retires) before the commit ends let
                // the key it then wrote void that instance's records (S1).
                drop(shared.storage().await);
                match exit {
                    Exit::Closed => return,
                    Exit::Idle => _ = node.metrics.evicted_idle.fetch_add(1, Relaxed),
                    Exit::Reset if shared.holds() => node.metrics.event("rolled-back", &shared.id, "a request failed", &shared.context.lock()),
                    Exit::Reset => node.metrics.event("lease-lost", &shared.id, "this node no longer holds the object's lease", &shared.context.lock()),
                    Exit::Poisoned(p, detail) => {
                        node.metrics.poison(p);
                        node.metrics.event(p.as_str(), &shared.id, detail, &shared.context.lock());
                    }
                }
            }
            Err(e) => {
                // A new object the app has no room for (`Objects::room`).
                let (status, reason) = match e.downcast_ref::<std::io::Error>().map(std::io::Error::kind) {
                    Some(std::io::ErrorKind::QuotaExceeded) => (507, "object-limit"),
                    Some(std::io::ErrorKind::StorageFull) => (507, "disk-full"),
                    _ => (503, "instantiate-failed"),
                };
                if status == 503 {
                    node.metrics.instantiate_failures.fetch_add(1, Relaxed);
                }
                let e = format!("{e:#}");
                node.metrics.event(reason, &shared.id, &*e, &shared.context.lock());
                let response = error_response(status, reason, &e);
                crate::telemetry::response(&mut job.span, &response);
                let _ = job.reply.send(response);
            }
        }
        drop(counted);
        drop(slot);
        shared.set_phase(Phase::Cold);
        if node.try_retire(&shared, &rx) {
            return;
        }
    }
}

async fn serve(
    node: &Arc<Node>,
    app: &App,
    shared: &Arc<ObjShared>,
    mut store: Store<ObjState>,
    svc: Service,
    first: Box<Job>,
    rx: &mut mpsc::Receiver<Box<Job>>,
) -> Exit {
    let limits = &app.cfg.limits;
    let max_resp = limits.max_response_body_kb << 10;
    let pending: Arc<Mutex<Option<Pending>>> = Arc::default();

    let inner = {
        let pending = pending.clone();
        let shared = shared.clone();
        let svc = &svc;
        let mut first = Some(first);
        async move |acc: &Accessor<ObjState>| -> wasmtime::Result<()> {
            loop {
                shared.set_phase(Phase::Idle);
                let mut job = if let Some(job) = first.take() { job } else {
                    let Some(mut job) = rx.recv().await else { return Ok(()) };
                    crate::telemetry::duration(&mut job.span, "nanovm.queue.duration_ms", job.enqueued);
                    job.span.set_attribute(opentelemetry::KeyValue::new("nanovm.cold_start", false));
                    job
                };
                *shared.context.lock() = job.context.clone();
                *shared.callers.lock() = std::mem::take(&mut job.callers);
                shared.begin_request();
                // The next tick, not one that passed while the object was
                // idle: a stale deadline made every request yield to the
                // executor at its first instruction, and Tokio wakes an idle
                // thread for a task that yields.
                acc.with(|mut s| s.as_context_mut().set_epoch_deadline(1));
                let abort = Arc::new(AtomicBool::new(false));
                *pending.lock() = Some(Pending {
                    reply: Some(job.reply),
                    abort: abort.clone(),
                    span: job.span,
                    phase: "nanovm.guest.duration_ms",
                    since: Instant::now(),
                });

                let (parts, body) = job.req.into_parts();
                let req = http::Request::from_parts(parts, Full::new(body));
                let (req, req_io) =
                    acc.with(|mut s| wasmtime_wasi_http::p3::Request::from_http(&mut s.data_mut().hooks, req));

                let (resp, mut failed) = match svc.handle(acc, req).await? {
                    Ok(res) => {
                        // The guest learns its answer is out (the future
                        // `response.new` gave it resolves) once the client
                        // has read it to its end, or gone: only then can its
                        // work after the answer fail without failing it.
                        let (sent, out) = oneshot::channel::<()>();
                        let out = async move { _ = out.await; req_io.await };
                        let mut res = acc.with(|mut s| res.into_http(&mut s, out))?;
                        // Only the node refuses requests to the proxy: a guest
                        // answering as one would have the proxy run its
                        // request again and again.
                        res.headers_mut().remove(crate::lease::OWNER);
                        let res = res.map(|b| {
                            GuardedBody {
                                inner: b,
                                remaining: max_resp,
                                abort,
                                sent: Some(sent),
                            }
                            .boxed_unsync()
                        });
                        (res, false)
                    }
                    Err(code) => (error_response(500, "guest-error", &format!("{code:?}")), true),
                };
                pending.lock().as_mut().unwrap().phase("nanovm.commit.duration_ms");
                shared.set_phase(Phase::PostReturn);
                // Nothing is heard of a request before its changes are
                // durable. One that failed (as one that traps or runs out of
                // time) restarts its instance, which rolls them back; an
                // answer, whatever its status, commits them.
                let resp = match failed {
                    false => match commit(acc).await {
                        Ok(()) => resp,
                        Err(e) => {
                            failed = true;
                            error_response(500, "commit-failed", &e.to_string())
                        }
                    },
                    true => resp,
                };
                {
                    let mut guard = pending.lock();
                    let p = guard.as_mut().unwrap();
                    crate::telemetry::response(&mut p.span, &resp);
                    crate::telemetry::duration(&mut p.span, "nanovm.response.duration_ms", job.enqueued);
                    p.phase("nanovm.post_return.duration_ms");
                    if let Some(tx) = p.reply.take() { let _ = tx.send(resp); }
                }
                // Its callers have their answer.
                shared.callers.lock().clear();
                // Let tasks the guest left running (e.g. streaming the body)
                // finish before taking the next request.
                poll_fn(|cx| acc.poll_no_interesting_tasks(cx)).await;
                pending.lock().as_mut().unwrap().phase("nanovm.post_return_commit.duration_ms");
                // What it changed after answering commits now.
                let reset = if failed { true } else {
                    match commit(acc).await {
                        Ok(()) => false,
                        Err(e) => {
                            pending.lock().as_mut().unwrap().span.set_attribute(opentelemetry::KeyValue::new(
                                "error.type", "post-return-commit-failed"));
                            node.metrics.event("post-return-commit-failed", &shared.id, e.to_string(), &job.context);
                            true
                        }
                    }
                };
                if let Some(mut p) = pending.lock().take() {
                    if reset { p.span.set_status(opentelemetry::trace::Status::error("request rolled back")); }
                }
                if reset {
                    acc.with(|mut s| s.data_mut().reset = true);
                    return Ok(());
                }
            }
        }
    };

    // Drive the store's event loop, charging the time spent inside each poll
    // to the object, and enforce the deadline of whatever phase it's in. This
    // runs outside the event loop because a guest spinning in a loop only
    // yields at epoch boundaries, never back into `inner`.
    let result: Result<wasmtime::Result<()>, Phase> = {
        let run = pin!(store.run_concurrent(inner));
        let mut fut = pin!(shared.charged(&node.metrics, run));
        let mut timer = pin!(tokio::time::sleep(Duration::from_secs(3600)));
        let mut armed = None;
        let wake = Arc::new(SelfWake { tick_ns: node.cfg.epoch_tick_ms * 1_000_000, ..Default::default() });
        let waker = Waker::from(wake.clone());
        poll_fn(|cx| {
            if let Poll::Ready(r) = wake.poll(cx, &waker, &shared.yielding, fut.as_mut()) {
                return Poll::Ready(Ok(r.and_then(|r| r)));
            }
            let t1 = now_ns();
            let (phase, since) = shared.phase();
            if armed != Some((phase, since)) {
                let ms = match phase {
                    Phase::Idle => limits.idle_timeout_ms,
                    Phase::PostReturn => limits.post_return_timeout_ms,
                    Phase::Handling | Phase::Starting | Phase::Cold => limits.request_timeout_ms,
                };
                let elapsed = Duration::from_nanos(t1.saturating_sub(since));
                let left = Duration::from_millis(ms).saturating_sub(elapsed);
                timer.as_mut().reset(tokio::time::Instant::now() + left);
                armed = Some((phase, since));
            }
            match timer.as_mut().poll(cx) {
                Poll::Ready(()) => Poll::Ready(Err(phase)),
                Poll::Pending => Poll::Pending,
            }
        })
        .await
    };

    let exit = match result {
        Ok(Ok(())) if store.data().reset => Exit::Reset,
        Ok(Ok(())) => Exit::Closed,
        Err(Phase::Idle) => Exit::Idle,
        Err(phase) => Exit::Poisoned(Poison::Timeout, format!("deadline exceeded while {}", phase.as_str())),
        Ok(Err(e)) => classify(&store, &e),
    };

    if let Exit::Poisoned(p, detail) = &exit
        && let Some(mut pending) = pending.lock().take()
    {
        pending.abort.store(true, Relaxed);
        pending.span.set_status(opentelemetry::trace::Status::error(p.as_str()));
        if let Some(tx) = pending.reply.take() {
            let status = if *p == Poison::Timeout { 504 } else { 500 };
            let response = error_response(status, p.as_str(), detail);
            crate::telemetry::response(&mut pending.span, &response);
            let _ = tx.send(response);
        } else {
            pending.span.set_attribute(opentelemetry::KeyValue::new("error.type", p.as_str()));
        }
    }
    drop(store);
    exit
}

/// The store's waker. Wasmtime wakes the actor during its own poll a few
/// times in an ordinary request (the guest returning, its task ending), and
/// Tokio takes a task that wakes itself for one that yields: it queues it
/// behind the others and wakes an idle thread to steal it, so each request
/// woke threads for nothing and moved between them. A wake during the poll
/// polls again at once instead.
///
/// So a poll holds its thread at most about a tick, as a guest that only
/// computes does: the epoch callback's yield always reaches Tokio, and so
/// does any wake once the poll has run for a tick, which bounds a loop of
/// self-wakes that runs no guest code too. A guest that wakes itself in a
/// tight loop (each wake goes back to the host, which runs it again) is
/// charged for it and killed at its CPU budget like a spinner.
#[derive(Default)]
struct SelfWake {
    /// `IDLE`, `POLLING`, or `WOKEN` during the poll.
    state: AtomicU8,
    task: Mutex<Option<Waker>>,
    tick_ns: u64,
}

const IDLE: u8 = 0;
const POLLING: u8 = 1;
const WOKEN: u8 = 2;

impl SelfWake {
    fn poll<F: Future>(&self, cx: &mut Context<'_>, waker: &Waker, yielding: &AtomicBool, mut fut: Pin<&mut F>) -> Poll<F::Output> {
        use std::sync::atomic::Ordering::SeqCst;
        {
            let mut task = self.task.lock();
            if !task.as_ref().is_some_and(|t| t.will_wake(cx.waker())) {
                *task = Some(cx.waker().clone());
            }
        }
        let start = now_ns();
        loop {
            self.state.store(POLLING, SeqCst);
            let poll = fut.as_mut().poll(&mut Context::from_waker(waker));
            if poll.is_ready() || self.state.swap(IDLE, SeqCst) != WOKEN {
                return poll;
            }
            if yielding.swap(false, Relaxed) || now_ns() - start >= self.tick_ns {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        }
    }
}

impl Wake for SelfWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        use std::sync::atomic::Ordering::SeqCst;
        if let Err(IDLE) = self.state.compare_exchange(POLLING, WOKEN, SeqCst, SeqCst)
            && let Some(task) = &*self.task.lock()
        {
            task.wake_by_ref();
        }
    }
}

fn classify(store: &Store<ObjState>, e: &wasmtime::Error) -> Exit {
    let st = store.data();
    let tail = st.stderr.text();
    let with_tail = |msg: String| if tail.is_empty() { msg } else { format!("{msg}\n{tail}") };
    if st.cpu_exceeded {
        let ms = st.cpu_budget_ns / 1_000_000;
        return Exit::Poisoned(Poison::Cpu, format!("used more than {ms}ms of CPU in one request"));
    }
    if st.limiter.denied {
        let mb = st.limiter.max_bytes >> 20;
        return Exit::Poisoned(
            Poison::Memory,
            with_tail(format!("tried to grow linear memory past {mb}MiB")),
        );
    }
    if e.downcast_ref::<ResourceTableError>().is_some() {
        return Exit::Poisoned(Poison::ResourceTable, "resource table full".into());
    }
    if let Some(trap) = e.downcast_ref::<Trap>() {
        return Exit::Poisoned(Poison::Trap, with_tail(format!("trap: {trap}")));
    }
    Exit::Poisoned(Poison::Trap, with_tail(format!("{e:#}")))
}

pub fn error_response(status: u16, reason: &str, detail: &str) -> http::Response<RespBody> {
    let body = serde_json::json!({ "error": reason, "detail": detail }).to_string();
    http::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("x-nanovm-error", reason)
        .body(Full::new(Bytes::from(body)).map_err(|e| match e {}).boxed_unsync())
        .unwrap()
}

/// Caps a guest's response body and lets the supervisor cut it off.
struct GuardedBody<B> {
    inner: B,
    remaining: u64,
    abort: Arc<AtomicBool>,
    /// Dropped once the body has been read to its end or failed, or with it.
    sent: Option<oneshot::Sender<()>>,
}

impl<B> Body for GuardedBody<B>
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: Into<BoxError>,
{
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        if self.abort.load(Relaxed) {
            return Poll::Ready(Some(Err("instance terminated".into())));
        }
        let poll = match Pin::new(&mut self.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    let n = data.len() as u64;
                    if n > self.remaining {
                        return Poll::Ready(Some(Err("response body exceeds max_response_body_kb".into())));
                    }
                    self.remaining -= n;
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e.into()))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        };
        if matches!(poll, Poll::Ready(None | Some(Err(_)))) {
            self.sent = None;
        }
        poll
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// What a previous run left in an app's scratch: an empty directory is
    /// handed out again as it is (not removed and made anew, which blocked
    /// a restart for milliseconds per directory), one with anything in it is
    /// removed, and new directories never take a name that is left.
    #[test]
    fn scratch_left_by_a_previous_run() {
        use std::os::unix::fs::MetadataExt;
        let dir = std::env::temp_dir().join(format!("nanovm-scratch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["7", "900", "x"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        std::fs::write(dir.join("x/f"), "left").unwrap();
        let ino = |p: &std::path::Path| std::fs::metadata(p).unwrap().ino();
        let mut left = [ino(&dir.join("7")), ino(&dir.join("900"))];
        left.sort();

        let scratch = Scratch::new(dir.clone()).unwrap();
        assert!(!dir.join("x").exists(), "a used directory is removed");
        let mut taken: Vec<_> = (0..3).map(|_| scratch.take().unwrap()).collect();
        let mut kept: Vec<_> = taken[..2].iter().map(|t| ino(&t.0)).collect();
        kept.sort();
        assert_eq!(kept, left, "the empty ones are reused first");
        let new = taken[2].0.file_name().unwrap().to_str().unwrap().parse::<u64>().unwrap();
        assert!(new > 900, "a new name is past those left: {new}");
        taken.clear();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The limiter caps the sum of every memory's growth; a refused growth
    /// is remembered and leaves the usage as it was.
    #[test]
    fn mem_limiter_caps_total_growth() {
        let mut l = MemLimiter {
            max_bytes: 100,
            used: 0,
            denied: false,
        };
        assert!(l.memory_growing(0, 60, None).unwrap());
        assert!(l.memory_growing(0, 40, None).unwrap(), "a second memory shares the cap");
        assert!(!l.denied);
        assert!(!l.memory_growing(60, 61, None).unwrap());
        assert!(l.denied);
        assert_eq!(l.used, 100);
        assert!(l.memory_growing(10, 5, None).unwrap(), "shrinking is free");
        assert_eq!(l.used, 100);
    }

    /// The log tail keeps the last `LOG_TAIL` bytes written, trimmed, with
    /// bytes that aren't UTF-8 replaced.
    #[tokio::test]
    async fn log_tail_keeps_the_end() {
        let tail = LogTail::default();
        let mut w = tail.clone();
        w.write_all(b"  early\n").await.unwrap();
        assert_eq!(tail.text(), "early");
        w.write_all(&[b'x'; LOG_TAIL - 4]).await.unwrap();
        w.write_all(b"end\n\xff").await.unwrap();
        let text = tail.text();
        assert!(text.starts_with("xxx") && text.ends_with("end\n\u{fffd}"), "{text:?}");
        assert_eq!(tail.buf.lock().len(), LOG_TAIL);
        w.write_all(&[b'y'; 3 * LOG_TAIL]).await.unwrap();
        assert_eq!(tail.text(), "y".repeat(LOG_TAIL));
    }

    /// A guest's body passes until it would exceed its cap, and fails once
    /// the instance is aborted.
    #[tokio::test]
    async fn guarded_body_caps_and_aborts() {
        let body = |b: &'static [u8], remaining, abort| GuardedBody {
            inner: Full::new(Bytes::from_static(b)),
            remaining,
            abort: Arc::new(AtomicBool::new(abort)),
            sent: None,
        };
        assert_eq!(body(b"12345", 5, false).collect().await.unwrap().to_bytes(), "12345");
        let e = body(b"123456", 5, false).collect().await.unwrap_err();
        assert!(e.to_string().contains("max_response_body_kb"));
        let e = body(b"1", 5, true).collect().await.unwrap_err();
        assert_eq!(e.to_string(), "instance terminated");
    }

    /// The guest hears that its answer is out once the body has been read
    /// to its end, though whoever read it keeps it.
    #[tokio::test]
    async fn guarded_body_says_when_it_has_been_read() {
        use oneshot::error::TryRecvError::{Closed, Empty};
        let (sent, mut out) = oneshot::channel::<()>();
        let inner = Full::new(Bytes::from_static(b"1"));
        let mut body = GuardedBody { inner, remaining: 5, abort: Arc::default(), sent: Some(sent) };
        assert!(body.frame().await.unwrap().is_ok());
        assert_eq!(out.try_recv(), Err(Empty));
        assert!(body.frame().await.is_none());
        assert_eq!(out.try_recv(), Err(Closed));
    }

    /// The task of an idle object holds little: once, moved-in futures held
    /// twice and an inline publish path made it 17 KB, every live object's.
    #[test]
    fn an_actor_is_small() {
        fn returned<A, B, C, D, F>(_: fn(A, B, C, D) -> F) -> usize {
            size_of::<F>()
        }
        let size = returned(run);
        assert!(size < 4096, "{size} bytes");
    }

    /// Errors are JSON naming the reason, which is also a header.
    #[tokio::test]
    async fn error_responses_are_json() {
        let res = error_response(503, "busy", "try \"later\"");
        assert_eq!(res.status(), 503);
        assert_eq!(res.headers()["content-type"], "application/json");
        assert_eq!(res.headers()["x-nanovm-error"], "busy");
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v, serde_json::json!({"error": "busy", "detail": "try \"later\""}));
    }

    /// Only CPU used inside a poll is charged: not waiting to be polled,
    /// nor blocking inside a poll, nor a commit; a new start resets the
    /// charge, and phases name themselves.
    #[tokio::test]
    async fn cpu_is_charged_only_inside_polls() {
        let spin = |ns| {
            let t0 = thread_cpu_ns();
            while thread_cpu_ns() - t0 < ns {}
        };
        let metrics = Metrics::default();
        let shared = ObjShared::new("a".into(), "o");
        assert_eq!(shared.phase().0, Phase::Cold);
        shared.begin_request();
        assert_eq!(shared.phase().0.as_str(), "handling");
        shared
            .charged(&metrics, pin!(tokio::time::sleep(Duration::from_millis(50))))
            .await;
        shared
            .charged(&metrics, pin!(async { std::thread::sleep(Duration::from_millis(50)) }))
            .await;
        let waited = shared.cpu_accum_ns.load(Relaxed);
        assert!(waited < 5_000_000, "waiting was charged {waited}ns");
        shared.charged(&metrics, pin!(async { spin(30_000_000) })).await;
        let used = shared.cpu_accum_ns.load(Relaxed);
        assert!(used >= 30_000_000);
        assert!(metrics.guest_cpu_ns.load(Relaxed) >= 30_000_000);
        let commit = async {
            let t0 = thread_cpu_ns();
            spin(30_000_000);
            shared.uncharge(t0);
        };
        shared.charged(&metrics, pin!(commit)).await;
        let committed = shared.cpu_accum_ns.load(Relaxed) - used;
        assert!(committed < 5_000_000, "a commit was charged {committed}ns");
        assert!(metrics.guest_cpu_ns.load(Relaxed) >= 60_000_000, "but counted as object CPU");
        shared.begin_start();
        assert!(shared.cpu_used_ns() < 5_000_000);
        let names = Phase::ALL.map(Phase::as_str);
        assert_eq!(names, ["starting", "idle", "handling", "post-return", "cold"]);
    }
}
