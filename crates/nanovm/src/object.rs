//! One durable object: an actor task that owns at most one live instance,
//! serves its queue one request at a time, and tears the instance down when it
//! goes idle or breaks a limit.

use std::collections::VecDeque;
use std::future::{Future, poll_fn};
use std::path::PathBuf;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering::Relaxed};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use http_body_util::{BodyExt, Full, combinators::UnsyncBoxBody};
use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use wasmtime::component::{Accessor, ResourceTable, ResourceTableError};
use wasmtime::{ResourceLimiter, Store, Trap, UpdateDeadline};
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::p3::bindings::Service;
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

use crate::app::App;
use crate::egress::EgressHooks;
use crate::metrics::{Poison, now_ns};
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
    const ALL: [Phase; 5] = [
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
    /// Guest CPU charged to the current request.
    cpu_accum_ns: AtomicU64,
    /// Start of the in-progress poll of the store, if any.
    poll_start_ns: AtomicU64,
    /// The heavy lane this object's request holds until its next yield.
    lane: Mutex<Option<OwnedSemaphorePermit>>,
}

impl ObjShared {
    pub fn new(app: Arc<str>, id: &str) -> Self {
        let now = now_ns();
        ObjShared {
            app,
            id: id.into(),
            phase: AtomicU8::new(Phase::Cold as u8),
            since_ns: now.into(),
            poll_start_ns: now.into(),
            ..Default::default()
        }
    }

    pub fn phase(&self) -> (Phase, u64) {
        (
            Phase::ALL[self.phase.load(Relaxed) as usize],
            self.since_ns.load(Relaxed),
        )
    }

    fn set_phase(&self, p: Phase) {
        if matches!(p, Phase::Idle | Phase::Cold) {
            self.lane.lock().take();
        }
        let now = now_ns();
        self.since_ns.store(now, Relaxed);
        self.phase.store(p as u8, Relaxed);
    }

    /// Starts a fresh CPU charge for instantiating a new instance. Without
    /// it the charge left by a killed request (or the time spent waiting
    /// for a slot) counted against the new instance's start-up.
    fn begin_start(&self) {
        self.cpu_accum_ns.store(0, Relaxed);
        self.poll_start_ns.store(now_ns(), Relaxed);
    }

    fn begin_request(&self) {
        self.begin_start();
        self.set_phase(Phase::Handling);
    }

    /// CPU charged to the current request, including the poll in progress.
    fn cpu_used_ns(&self) -> u64 {
        let start = self.poll_start_ns.load(Relaxed);
        self.cpu_accum_ns.load(Relaxed) + now_ns().saturating_sub(start)
    }
}

/// Caps the sum of all linear memories of one instance.
pub struct MemLimiter {
    max_bytes: usize,
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
pub struct LogTail(Arc<Mutex<VecDeque<u8>>>);

const LOG_TAIL: usize = 2048;

impl LogTail {
    pub fn text(&self) -> String {
        let buf: Vec<u8> = self.0.lock().iter().copied().collect();
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
        let mut buf = self.0.lock();
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

/// Removes an instance's scratch directory when the instance goes away.
struct TmpDir(PathBuf);

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub struct ObjState {
    wasi: WasiCtx,
    table: ResourceTable,
    http: WasiHttpCtx,
    hooks: EgressHooks,
    limiter: MemLimiter,
    shared: Arc<ObjShared>,
    cpu_budget_ns: u64,
    cpu_exceeded: bool,
    stderr: LogTail,
    /// The app's shared `/app` preopen; see `App::static_dir`.
    pub app_dir: Option<wasmtime_wasi::filesystem::Dir>,
    pub quota: QuotaGuard,
    _tmp: TmpDir,
}

impl Drop for ObjState {
    fn drop(&mut self) {
        self.hooks.metrics.stores_live.fetch_sub(1, Relaxed);
    }
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

fn new_store(node: &Node, app: &App, shared: &Arc<ObjShared>) -> anyhow::Result<Store<ObjState>> {
    let limits = &app.cfg.limits;
    let n = node.next_seq();
    let data_dir = node.cfg.data_dir.join("objects").join(&*app.name).join(&*shared.id);
    let tmp_dir = node
        .cfg
        .data_dir
        .join("tmp")
        .join(&*app.name)
        .join(format!("{}.{n}", shared.id));
    std::fs::create_dir_all(&data_dir)?;
    std::fs::create_dir_all(&tmp_dir)?;
    let tmp = TmpDir(tmp_dir.clone());

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
    b.preopened_dir(&tmp_dir, "/tmp", FsPerms::ReadWrite)?;
    b.stdout(stderr.clone()).stderr(stderr.clone());

    let mut table = ResourceTable::new();
    table.set_max_capacity(limits.max_resources);

    // `/tmp` was just created, so only the persistent volume holds usage.
    let quota = QuotaGuard::new(&data_dir, limits, node.metrics.clone(), shared.clone())?;

    let live = node.metrics.stores_live.fetch_add(1, Relaxed) + 1;
    node.metrics.stores_peak.fetch_max(live, Relaxed);
    let state = ObjState {
        wasi: b.build(),
        table,
        http: WasiHttpCtx::new(),
        hooks: EgressHooks {
            policy: app.egress.clone(),
            sem: Arc::new(Semaphore::new(limits.max_outbound_concurrency)),
            metrics: node.metrics.clone(),
            obj: shared.clone(),
        },
        limiter: MemLimiter {
            max_bytes: (limits.memory_mb << 20) as usize,
            used: 0,
            denied: false,
        },
        shared: shared.clone(),
        cpu_budget_ns: limits.cpu_ms * 1_000_000,
        cpu_exceeded: false,
        stderr,
        app_dir: app.static_dir.clone(),
        quota,
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
        // Hand back the lane at every tick so heavy requests take turns.
        st.shared.lane.lock().take();
        if used < HEAVY_AFTER_NS {
            return Ok(UpdateDeadline::Yield(1));
        }
        let (heavy, shared) = (heavy.clone(), st.shared.clone());
        Ok(UpdateDeadline::YieldCustom(
            1,
            Box::pin(async move {
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
    Poisoned(Poison, String),
}

/// Reply slot for the request in flight, shared between the request loop
/// (inside the store's event loop) and the supervisor (outside it), which
/// answers on the guest's behalf if the instance has to be killed.
struct Pending {
    reply: Option<oneshot::Sender<http::Response<RespBody>>>,
    abort: Arc<AtomicBool>,
}

pub async fn run(node: Arc<Node>, app: Arc<App>, shared: Arc<ObjShared>, mut rx: mpsc::Receiver<Job>) {
    loop {
        shared.set_phase(Phase::Starting);
        // A store holds a slot until it is dropped, and the pool has room for
        // every slot. An evicted object's store lives until its actor next
        // runs, so a new object waits here until that store is gone rather
        // than finding the pool full.
        let slot = node.slots.clone().acquire_owned().await.expect("slots is never closed");
        shared.begin_start();
        let t0 = Instant::now();
        let started = async {
            let mut store = new_store(&node, &app, &shared)?;
            let svc = app.pre.instantiate_async(&mut store).await?;
            anyhow::Ok((store, svc))
        }
        .await;
        match started {
            Ok((store, svc)) => {
                node.metrics.instantiate.record(t0.elapsed());
                match serve(&node, &app, &shared, store, svc, &mut rx).await {
                    Exit::Closed => return,
                    Exit::Idle => _ = node.metrics.evicted_idle.fetch_add(1, Relaxed),
                    Exit::Poisoned(p, detail) => {
                        node.metrics.poison(p);
                        node.metrics.event(p.as_str(), &app.name, &shared.id, detail);
                    }
                }
            }
            Err(e) => {
                let e = format!("{e:#}");
                node.metrics.instantiate_failures.fetch_add(1, Relaxed);
                node.metrics.event("instantiate-failed", &app.name, &shared.id, &*e);
                if let Ok(job) = rx.try_recv() {
                    let _ = job.reply.send(error_response(503, "instantiate-failed", &e));
                }
            }
        }
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
    rx: &mut mpsc::Receiver<Job>,
) -> Exit {
    let limits = &app.cfg.limits;
    let max_resp = limits.max_response_body_kb << 10;
    let pending: Arc<Mutex<Option<Pending>>> = Arc::default();
    let metrics = node.metrics.clone();

    let inner = {
        let pending = pending.clone();
        let shared = shared.clone();
        let svc = &svc;
        async move |acc: &Accessor<ObjState>| -> wasmtime::Result<()> {
            loop {
                shared.set_phase(Phase::Idle);
                let Some(job) = rx.recv().await else { return Ok(()) };
                shared.begin_request();
                metrics.requests.fetch_add(1, Relaxed);
                let abort = Arc::new(AtomicBool::new(false));
                *pending.lock() = Some(Pending {
                    reply: Some(job.reply),
                    abort: abort.clone(),
                });

                let (parts, body) = job.req.into_parts();
                let req = http::Request::from_parts(parts, Full::new(body));
                let (req, req_io) =
                    acc.with(|mut s| wasmtime_wasi_http::p3::Request::from_http(&mut s.data_mut().hooks, req));

                let resp = match svc.handle(acc, req).await? {
                    Ok(res) => {
                        let res = acc.with(|mut s| res.into_http(&mut s, req_io))?;
                        res.map(|b| {
                            GuardedBody {
                                inner: b,
                                remaining: max_resp,
                                abort,
                            }
                            .boxed_unsync()
                        })
                    }
                    Err(code) => error_response(500, "guest-error", &format!("{code:?}")),
                };
                shared.set_phase(Phase::PostReturn);
                let status = resp.status().as_u16();
                if let Some(tx) = pending.lock().as_mut().and_then(|p| p.reply.take()) {
                    let _ = tx.send(resp);
                }
                metrics.status(status);
                metrics.latency.record(job.enqueued.elapsed());

                // Let tasks the guest left running (e.g. streaming the body)
                // finish before taking the next request.
                poll_fn(|cx| acc.poll_no_interesting_tasks(cx)).await;
                *pending.lock() = None;
            }
        }
    };

    // Drive the store's event loop, charging the time spent inside each poll
    // to the object, and enforce the deadline of whatever phase it's in. This
    // runs outside the event loop because a guest spinning in a loop only
    // yields at epoch boundaries, never back into `inner`.
    let result: Result<wasmtime::Result<()>, Phase> = {
        let mut fut = pin!(store.run_concurrent(inner));
        let mut timer = pin!(tokio::time::sleep(Duration::from_secs(3600)));
        let mut armed = None;
        poll_fn(|cx| {
            let t0 = now_ns();
            shared.poll_start_ns.store(t0, Relaxed);
            let poll = fut.as_mut().poll(cx);
            let t1 = now_ns();
            let start = shared.poll_start_ns.load(Relaxed);
            shared.cpu_accum_ns.fetch_add(t1.saturating_sub(start), Relaxed);
            node.metrics.guest_cpu_ns.fetch_add(t1 - t0, Relaxed);
            if let Poll::Ready(r) = poll {
                return Poll::Ready(Ok(r.and_then(|r| r)));
            }
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
        Ok(Ok(())) => Exit::Closed,
        Err(Phase::Idle) => Exit::Idle,
        Err(phase) => Exit::Poisoned(Poison::Timeout, format!("deadline exceeded while {}", phase.as_str())),
        Ok(Err(e)) => classify(&store, &e),
    };

    if let Exit::Poisoned(p, detail) = &exit
        && let Some(mut pending) = pending.lock().take()
    {
        pending.abort.store(true, Relaxed);
        if let Some(tx) = pending.reply.take() {
            let status = if *p == Poison::Timeout { 504 } else { 500 };
            node.metrics.status(status);
            let _ = tx.send(error_response(status, p.as_str(), detail));
        }
    }
    drop(store);
    exit
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
        match Pin::new(&mut self.inner).poll_frame(cx) {
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
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}
