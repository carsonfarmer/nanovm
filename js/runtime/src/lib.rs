//! nanovm's JavaScript runtime: a `wasi:http` handler that runs a
//! Workers-style module (`export default { fetch(request, env, ctx) }`) on the
//! Boa engine. The app's source is compiled in (see build.rs). `prelude.js`
//! builds the Web APIs (Request, Response, Headers, fetch, env) on the few
//! host functions here; this file drives the engine's jobs and maps the
//! host's async calls (outgoing HTTP, timers) to promises.
//!
//! The engine lives as long as the instance, so module state persists
//! between requests the way an object's memory does, and is lost with it.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::rc::Rc;
use std::task::Poll;

use boa_engine::builtins::promise::PromiseState;
use boa_engine::context::time::{Clock, JsDuration, JsInstant};
use boa_engine::job::{GenericJob, Job, JobExecutor, NativeAsyncJob, PromiseJob};
use boa_engine::object::ObjectInitializer;
use boa_engine::object::builtins::{JsArray, JsFunction, JsPromise, JsUint8Array};
use boa_engine::{
    Context, JsArgs, JsError, JsResult, JsString, JsValue, Module, NativeFunction, Source, js_error, js_string,
};
use wasip3::http::types::{ErrorCode, Fields, Method, Request, Response, Scheme};
use wasip3::{wit_future, wit_stream};

mod kv {
    wit_bindgen::generate!({ path: "../../wit/keyvalue", world: "wasi:keyvalue/imports", generate_all });
    pub use wasi::keyvalue::store::*;
}

mod config {
    wit_bindgen::generate!({ path: "../../wit/config", world: "wasi:config/imports", generate_all });
    pub use wasi::config::store::*;
}

wasip3::http::service::export!(Js);

/// Pre-initialization (js/build.sh runs it through Wizer): starts the
/// engine and evaluates the app, so the component's memory starts there
/// and a cold start skips it. It must not touch the host.
mod init {
    wit_bindgen::generate!({
        inline: "package nanovm:js; world init { export wizer-initialize: func() -> result<_, string>; }",
    });
    export!(Init);
    struct Init;
    impl Guest for Init {
        fn wizer_initialize() -> Result<(), String> {
            super::Runtime::get().map(|_| ()).map_err(|e| e.to_string())
        }
    }
}

struct Js;

const APP: &str = include_str!(concat!(env!("OUT_DIR"), "/app.js"));
const PRELUDE: &str = include_str!("prelude.js");

thread_local! {
    static RUNTIME: Cell<Option<&'static Runtime>> = const { Cell::new(None) };
}

type Headers = Vec<(String, Vec<u8>)>;

impl wasip3::exports::http::handler::Guest for Js {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let (method, url, headers, body) = read(request).await;
        let rt = Runtime::get().map_err(|e| uncaught(e.to_string()))?;
        let id = std::env::var("NANOVM_OBJECT_ID").unwrap_or_default();
        let answer = rt.serve(&id, method, url, headers, body).await;
        let (status, headers, body, after) = answer.map_err(|e| uncaught(rt.describe(&e)))?;
        // `ctx.waitUntil` work goes on after the answer, before the next
        // request; anything else still pending is dropped.
        match after {
            Some(after) => wasip3::spawn_local(async move {
                if let Err(e) = rt.settle(&after).await {
                    eprintln!("waitUntil: {}", rt.describe(&e));
                }
                rt.cancel();
            }),
            None => rt.cancel(),
        }
        respond(status, headers, body)
    }
}

/// An uncaught exception fails the request, which rolls back its changes.
fn uncaught(msg: String) -> ErrorCode {
    eprintln!("Uncaught {msg}");
    ErrorCode::InternalError(Some(format!("Uncaught {msg}")))
}

/// Async jobs (host calls, timers) in flight.
type Task = Pin<Box<dyn Future<Output = JsResult<JsValue>>>>;

/// The engine and the app, for the instance's life (so leaked: the
/// engine's async jobs borrow it for as long as they run).
struct Runtime {
    ctx: &'static RefCell<&'static mut Context>,
    jobs: Rc<Jobs>,
    /// The prelude's `(id, method, url, headers, body) => Promise<[status, headers, body, after]>`.
    dispatch: JsFunction,
    running: RefCell<Vec<Task>>,
}

impl Runtime {
    fn get() -> JsResult<&'static Self> {
        if let Some(rt) = RUNTIME.get() {
            return Ok(rt);
        }
        let rt = Box::leak(Box::new(Self::new()?));
        RUNTIME.set(Some(rt));
        Ok(rt)
    }

    fn new() -> JsResult<Self> {
        let jobs = Rc::new(Jobs::default());
        let ctx = Context::builder().job_executor(jobs.clone()).clock(Rc::new(HostClock)).build()?;
        let ctx = Box::leak(Box::new(ctx));
        boa_runtime::register(boa_runtime::extensions::ConsoleExtension(boa_runtime::DefaultLogger), None, ctx)?;
        let host = ObjectInitializer::new(ctx)
            .function(NativeFunction::from_fn_ptr(kv_get), js_string!("kvGet"), 1)
            .function(NativeFunction::from_fn_ptr(kv_set), js_string!("kvSet"), 2)
            .function(NativeFunction::from_fn_ptr(kv_delete), js_string!("kvDelete"), 1)
            .function(NativeFunction::from_fn_ptr(kv_list), js_string!("kvList"), 1)
            .function(NativeFunction::from_fn_ptr(config_all), js_string!("configAll"), 0)
            .function(NativeFunction::from_fn_ptr(random), js_string!("random"), 1)
            .function(NativeFunction::from_async_fn(send), js_string!("send"), 6)
            .build();
        let prelude = ctx.eval(Source::from_bytes(PRELUDE))?;
        let bind = callable(&prelude)?.call(&JsValue::undefined(), &[host.into()], ctx)?;

        let module = Module::parse(Source::from_bytes(APP), None, ctx)?;
        let loaded = module.load_link_evaluate(ctx);
        jobs.clone().run_jobs(ctx)?;
        match loaded.state() {
            PromiseState::Fulfilled(_) => {}
            PromiseState::Rejected(e) => return Err(JsError::from_opaque(e)),
            PromiseState::Pending => return Err(js_error!(Error: "the module's top-level code must not wait for I/O")),
        }
        let app = module.namespace(ctx).get(js_string!("default"), ctx)?;
        let dispatch = callable(&callable(&bind)?.call(&JsValue::undefined(), &[app], ctx)?)?;
        let ctx = Box::leak(Box::new(RefCell::new(ctx)));
        Ok(Self { ctx, jobs, dispatch, running: RefCell::default() })
    }

    async fn serve(
        &'static self,
        id: &str,
        method: String,
        url: String,
        headers: Headers,
        body: Vec<u8>,
    ) -> JsResult<(u16, Headers, Vec<u8>, Option<JsPromise>)> {
        let answer = {
            let ctx = &mut self.ctx.borrow_mut();
            let args = [
                JsString::from(id).into(),
                JsString::from(method).into(),
                JsString::from(url).into(),
                to_js_headers(headers, ctx)?,
                if body.is_empty() { JsValue::null() } else { JsUint8Array::from_iter(body, ctx)?.into() },
            ];
            promise(&self.dispatch.call(&JsValue::undefined(), &args, ctx)?)?
        };
        let answer = self.settle(&answer).await?;
        let ctx = &mut self.ctx.borrow_mut();
        let answer = JsArray::from_object(object(&answer)?)?;
        let status = answer.get(0, ctx)?.to_u32(ctx)? as u16;
        let headers = from_js_headers(&answer.get(1, ctx)?, ctx)?;
        let body = bytes(&answer.get(2, ctx)?, ctx)?;
        let after = answer.get(3, ctx)?;
        let after = if after.is_undefined() { None } else { Some(promise(&after)?) };
        Ok((status, headers, body, after))
    }

    /// Runs the engine's jobs until `promise` settles, waiting on the host
    /// whenever only host calls (and timers) are left.
    async fn settle(&'static self, promise: &JsPromise) -> JsResult<JsValue> {
        loop {
            self.jobs.clone().run_jobs(&mut self.ctx.borrow_mut())?;
            match promise.state() {
                PromiseState::Fulfilled(v) => return Ok(v),
                PromiseState::Rejected(e) => return Err(JsError::from_opaque(e)),
                PromiseState::Pending => {}
            }
            let mut running = self.running.borrow_mut();
            running.extend(self.jobs.tasks.take().into_iter().map(|t| Box::pin(t.call(self.ctx)) as Task));
            if running.is_empty() {
                return Err(js_error!(Error: "a promise the request waits on can never settle"));
            }
            poll_fn(|cx| {
                let mut done = Ok(false);
                running.retain_mut(|t| match t.as_mut().poll(cx) {
                    Poll::Ready(r) => {
                        done = done.clone().and(r.map(|_| true));
                        false
                    }
                    Poll::Pending => true,
                });
                match done {
                    Ok(false) => Poll::Pending,
                    d => Poll::Ready(d),
                }
            })
            .await?;
        }
    }

    /// Drops the work still pending: a request's work ends with its answer
    /// (and its `waitUntil` promises).
    fn cancel(&self) {
        self.running.take();
        self.jobs.tasks.take();
    }

    fn describe(&self, e: &JsError) -> String {
        match e.try_native(&mut self.ctx.borrow_mut()) {
            Ok(native) => native.to_string(),
            Err(_) => e.to_string(),
        }
    }
}

/// The engine's job queue. Host calls and timers are async jobs, which
/// [`Runtime::settle`] polls.
#[derive(Default)]
struct Jobs {
    promises: RefCell<VecDeque<PromiseJob>>,
    generic: RefCell<VecDeque<GenericJob>>,
    tasks: RefCell<Vec<NativeAsyncJob>>,
}

impl JobExecutor for Jobs {
    fn enqueue_job(self: Rc<Self>, job: Job, _: &mut Context) {
        let task = match job {
            Job::PromiseJob(j) => return self.promises.borrow_mut().push_back(j),
            Job::GenericJob(j) => return self.generic.borrow_mut().push_back(j),
            Job::AsyncJob(j) => j,
            Job::TimeoutJob(t) => NativeAsyncJob::new(async move |ctx| {
                sleep(t.timeout()).await;
                if t.cancelled() { Ok(JsValue::undefined()) } else { t.call(&mut ctx.borrow_mut()) }
            }),
            Job::IntervalJob(t) => NativeAsyncJob::new(async move |ctx| {
                loop {
                    sleep(t.interval()).await;
                    if t.cancelled() {
                        return Ok(JsValue::undefined());
                    }
                    t.call(&mut ctx.borrow_mut())?;
                }
            }),
            // FinalizationRegistry callbacks never run, which the language allows.
            _ => return,
        };
        self.tasks.borrow_mut().push(task);
    }

    fn run_jobs(self: Rc<Self>, ctx: &mut Context) -> JsResult<()> {
        loop {
            let promise = self.promises.borrow_mut().pop_front();
            if let Some(job) = promise {
                job.call(ctx)?;
                continue;
            }
            let generic = self.generic.borrow_mut().pop_front();
            match generic {
                Some(job) => job.call(ctx)?,
                None => break,
            };
        }
        ctx.clear_kept_objects();
        Ok(())
    }
}

/// Reads the host's clocks at each call (Boa's own clock reads one when the
/// engine starts, which pre-initialization would freeze).
struct HostClock;

impl Clock for HostClock {
    fn now(&self) -> JsInstant {
        let ns = wasip3::clocks::monotonic_clock::now();
        JsInstant::new(ns / 1_000_000_000, (ns % 1_000_000_000) as u32)
    }

    fn system_time_millis(&self) -> i64 {
        let t = wasip3::clocks::system_clock::now();
        t.seconds * 1000 + i64::from(t.nanoseconds / 1_000_000)
    }
}

async fn sleep(d: JsDuration) {
    wasip3::clocks::monotonic_clock::wait_for(d.as_nanos() as u64).await;
}

// ---- values between Rust and JavaScript --------------------------------

fn object(v: &JsValue) -> JsResult<boa_engine::JsObject> {
    v.as_object().ok_or_else(|| js_error!(TypeError: "expected an object"))
}

fn callable(v: &JsValue) -> JsResult<JsFunction> {
    JsFunction::from_object(object(v)?).ok_or_else(|| js_error!(TypeError: "expected a function"))
}

fn promise(v: &JsValue) -> JsResult<JsPromise> {
    JsPromise::from_object(object(v)?)
}

fn string(v: &JsValue, ctx: &mut Context) -> JsResult<String> {
    Ok(v.to_string(ctx)?.to_std_string_escaped())
}

/// A `Uint8Array`'s bytes; `null` or `undefined` is empty.
fn bytes(v: &JsValue, ctx: &mut Context) -> JsResult<Vec<u8>> {
    if v.is_null_or_undefined() {
        return Ok(Vec::new());
    }
    JsUint8Array::from_object(object(v)?)?.to_vec(ctx)
}

/// Headers cross as a flat array, `[name, value, name, value, …]`.
fn to_js_headers(headers: Headers, ctx: &mut Context) -> JsResult<JsValue> {
    let flat = headers.into_iter().flat_map(|(k, v)| {
        [JsString::from(k).into(), JsString::from(String::from_utf8_lossy(&v).as_ref()).into()]
    });
    Ok(JsArray::from_iter(flat, ctx).into())
}

fn from_js_headers(v: &JsValue, ctx: &mut Context) -> JsResult<Headers> {
    let flat = JsArray::from_object(object(v)?)?;
    let n = flat.length(ctx)?;
    (0..n / 2)
        .map(|i| Ok((string(&flat.get(2 * i, ctx)?, ctx)?, string(&flat.get(2 * i + 1, ctx)?, ctx)?.into_bytes())))
        .collect()
}

// ---- host functions ------------------------------------------------------

thread_local! {
    /// Opened at first use, at run time: no handle may be in the snapshot.
    static BUCKET: std::cell::OnceCell<kv::Bucket> = const { std::cell::OnceCell::new() };
}

fn with_bucket<T>(f: impl FnOnce(&kv::Bucket) -> Result<T, kv::Error>) -> JsResult<T> {
    BUCKET
        .with(|cell| {
            if cell.get().is_none() {
                let _ = cell.set(kv::open("")?);
            }
            f(cell.get().unwrap())
        })
        .map_err(kv_error)
}

fn kv_error(e: kv::Error) -> JsError {
    js_error!(Error: "kv: {:?}", e)
}

fn kv_get(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let key = string(args.get_or_undefined(0), ctx)?;
    match with_bucket(|b| b.get(&key))? {
        Some(v) => Ok(JsUint8Array::from_iter(v, ctx)?.into()),
        None => Ok(JsValue::undefined()),
    }
}

fn kv_set(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let key = string(args.get_or_undefined(0), ctx)?;
    let value = bytes(args.get_or_undefined(1), ctx)?;
    with_bucket(|b| b.set(&key, &value))?;
    Ok(JsValue::undefined())
}

fn kv_delete(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let key = string(args.get_or_undefined(0), ctx)?;
    with_bucket(|b| b.delete(&key))?;
    Ok(JsValue::undefined())
}

/// `kvList(cursor)`: `[keys, next cursor or undefined]`.
fn kv_list(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let cursor = args.get_or_undefined(0);
    let cursor = if cursor.is_undefined() { None } else { Some(cursor.to_number(ctx)? as u64) };
    let page = with_bucket(|b| b.list_keys(cursor))?;
    let keys = JsArray::from_iter(page.keys.into_iter().map(|k| JsString::from(k).into()), ctx);
    let next = page.cursor.map_or(JsValue::undefined(), |c| JsValue::from(c as f64));
    Ok(JsArray::from_iter([keys.into(), next], ctx).into())
}

fn config_all(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let all = config::get_all().map_err(|e| js_error!(Error: "config: {:?}", e))?;
    let flat = all.into_iter().flat_map(|(k, v)| [JsString::from(k).into(), JsString::from(v).into()]);
    Ok(JsArray::from_iter(flat, ctx).into())
}

fn random(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let n = args.get_or_undefined(0).to_index(ctx)?;
    Ok(JsUint8Array::from_iter(wasip3::random::random::get_random_bytes(n), ctx)?.into())
}

/// `send(method, scheme, authority, path, headers, body)`: an outgoing
/// request through the host (so under the app's egress policy); resolves to
/// `[status, headers, body]`.
async fn send(_: &JsValue, args: &[JsValue], ctx: &RefCell<&mut Context>) -> JsResult<JsValue> {
    let (parts, headers, body) = {
        let ctx = &mut ctx.borrow_mut();
        let mut parts = [const { String::new() }; 4];
        for (i, p) in parts.iter_mut().enumerate() {
            *p = string(args.get_or_undefined(i), ctx)?;
        }
        (parts, from_js_headers(args.get_or_undefined(4), ctx)?, bytes(args.get_or_undefined(5), ctx)?)
    };
    let [method, scheme, authority, path] = parts;
    let url = format!("{scheme}://{authority}{path}");
    let (status, headers, body) = outgoing(method, scheme, authority, path, headers, body)
        .await
        .map_err(|e| js_error!(TypeError: "fetch {}: {:?}", url, e))?;
    let ctx = &mut ctx.borrow_mut();
    let answer = [JsValue::from(status), to_js_headers(headers, ctx)?, JsUint8Array::from_iter(body, ctx)?.into()];
    Ok(JsArray::from_iter(answer, ctx).into())
}

// ---- wasi:http -------------------------------------------------------------

fn internal(what: impl std::fmt::Debug) -> ErrorCode {
    ErrorCode::InternalError(Some(format!("{what:?}")))
}

async fn read(request: Request) -> (String, String, Headers, Vec<u8>) {
    let method = match request.get_method() {
        Method::Get => "GET".into(),
        Method::Head => "HEAD".into(),
        Method::Post => "POST".into(),
        Method::Put => "PUT".into(),
        Method::Delete => "DELETE".into(),
        Method::Connect => "CONNECT".into(),
        Method::Options => "OPTIONS".into(),
        Method::Trace => "TRACE".into(),
        Method::Patch => "PATCH".into(),
        Method::Other(m) => m,
    };
    let scheme = match request.get_scheme() {
        Some(Scheme::Https) => "https",
        _ => "http",
    };
    let authority = request.get_authority().unwrap_or_else(|| "localhost".into());
    let path = request.get_path_with_query().unwrap_or_else(|| "/".into());
    let headers = request.get_headers().copy_all();
    let (done_tx, done_rx) = wit_future::new(|| Ok(()));
    let (body, _trailers) = Request::consume_body(request, done_rx);
    let body = body.collect().await;
    drop(done_tx);
    (method, format!("{scheme}://{authority}{path}"), headers, body)
}

/// A response whose body a spawned task writes once the host has it.
fn respond(status: u16, mut headers: Headers, body: Vec<u8>) -> Result<Response, ErrorCode> {
    headers.retain(|(k, _)| !k.eq_ignore_ascii_case("content-length"));
    headers.push(("content-length".into(), body.len().to_string().into_bytes()));
    let fields = Fields::from_list(&headers).map_err(internal)?;
    let (mut body_tx, body_rx) = wit_stream::new();
    let (trailers_tx, trailers_rx) = wit_future::new(|| Ok(None));
    let (response, _done) = Response::new(fields, Some(body_rx), trailers_rx);
    drop(trailers_tx);
    response.set_status_code(status).map_err(|()| internal(format!("status {status}")))?;
    wasip3::spawn_local(async move {
        let _ = body_tx.write_all(body).await;
    });
    Ok(response)
}

async fn outgoing(
    method: String,
    scheme: String,
    authority: String,
    path: String,
    headers: Headers,
    body: Vec<u8>,
) -> Result<(u16, Headers, Vec<u8>), ErrorCode> {
    let fields = Fields::from_list(&headers).map_err(internal)?;
    let (mut body_tx, body_rx) = wit_stream::new();
    let (trailers_tx, trailers_rx) = wit_future::new(|| Ok(None));
    let (request, _sent) = Request::new(fields, Some(body_rx), trailers_rx, None);
    drop(trailers_tx);
    let method = match method.as_str() {
        "GET" => Method::Get,
        "HEAD" => Method::Head,
        "POST" => Method::Post,
        "PUT" => Method::Put,
        "DELETE" => Method::Delete,
        "PATCH" => Method::Patch,
        "OPTIONS" => Method::Options,
        "CONNECT" => Method::Connect,
        "TRACE" => Method::Trace,
        _ => Method::Other(method),
    };
    let scheme = match scheme.as_str() {
        "http" => Scheme::Http,
        "https" => Scheme::Https,
        _ => Scheme::Other(scheme),
    };
    request.set_method(&method).map_err(|()| internal("method"))?;
    request.set_scheme(Some(&scheme)).map_err(|()| internal("scheme"))?;
    request.set_authority(Some(&authority)).map_err(|()| internal("authority"))?;
    request.set_path_with_query(Some(&path)).map_err(|()| internal("path"))?;
    wasip3::spawn_local(async move {
        let _ = body_tx.write_all(body).await;
    });
    let response = wasip3::http::client::send(request).await?;
    let status = response.get_status_code();
    let headers = response.get_headers().copy_all();
    let (done_tx, done_rx) = wit_future::new(|| Ok(()));
    let (body, _trailers) = Response::consume_body(response, done_rx);
    let body = body.collect().await;
    drop(done_tx);
    Ok((status, headers, body))
}
