//! nanovm's JavaScript runtime: a guest-SDK app that runs a Workers-style
//! module (`export default { fetch(request, env, ctx) }`) on QuickJS
//! (QuickJS-NG, through rquickjs). The app's source is compiled in (see
//! build.rs). `prelude.js` builds the Web APIs (Request, Response, Headers,
//! URL, fetch, timers, env) on the host functions here; this file runs the
//! engine's jobs and maps the host's async calls (outgoing HTTP, timers) to
//! JavaScript callbacks.
//!
//! The engine lives as long as the instance, so module state persists
//! between requests the way an object's memory does, and is lost with it.

use std::cell::{Cell, OnceCell, RefCell};
use std::ffi::c_void;
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::task::Poll;

use nanovm_sdk::wasip3::clocks::{monotonic_clock, system_clock};
use nanovm_sdk::wasip3::random::random;
use nanovm_sdk::{Error, Request, Response, kv};
use rquickjs::context::intrinsic::{
    Date, Eval, Json, MapSet, Promise as Promises, Proxy, RegExp, RegExpCompiler, TypedArrays, WeakRef,
};
use rquickjs::convert::List;
use rquickjs::{
    Array, Coerced, Context, Ctx, Exception, Function, IntoJs, Module, Object, Persistent, Promise, Runtime, TypedArray,
    Value,
};
use url::{Url, quirks};

nanovm_sdk::serve!(handle);

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
            super::engine().map(|_| ())
        }
    }
}

const APP: &str = include_str!(concat!(env!("OUT_DIR"), "/app.js"));
const PRELUDE: &str = include_str!("prelude.js");

async fn handle(request: Request) -> nanovm_sdk::Result {
    let js = engine().map_err(uncaught)?;
    let (status, headers, body, after) = js.serve(request).await.map_err(uncaught)?;
    // `ctx.waitUntil` work goes on as Rust's `after` work, once the answer
    // is out, until every promise has settled (those registered meanwhile
    // too), before the next request; anything else still pending is
    // dropped. What it changes from then on commits when it is done. If it
    // fails (a promise rejects, whenever it did, or they can never settle),
    // so does that work, as when `after` work panics: the instance stops,
    // what it changed after the answer rolls back, and the answer stands
    // with what was changed before it.
    match after {
        Some(after) => nanovm_sdk::after(async move {
            if let Err(e) = js.settle(after).await {
                eprintln!("Uncaught (in waitUntil) {e}");
                std::process::abort();
            }
            TASKS.take();
        }),
        None => drop(TASKS.take()),
    }
    Ok(headers.chunks_exact(2).fold(Response::new(status).body(body), |r, kv| r.header(&kv[0], &kv[1])))
}

/// An uncaught exception fails the request, which rolls back its changes.
fn uncaught(e: String) -> Error {
    eprintln!("Uncaught {e}");
    Error::new(format!("Uncaught {e}"))
}

/// The engine and the app, for the instance's life.
struct Engine {
    rt: Runtime,
    ctx: Context,
    /// The prelude's `serve(id, method, url, headers, body)`: `[the app's
    /// response or a promise of it, ctx]`.
    serve: Persistent<Function<'static>>,
    /// The prelude's `answer(response, ctx)`: `[status, headers, body, waitUntil promise or undefined]`.
    answer: Persistent<Function<'static>>,
}

/// A host call in flight (an outgoing request, a timer). Once the host
/// answers, it returns what to do in JavaScript with the answer.
type Task = Pin<Box<dyn Future<Output = Settle>>>;
type Settle = Box<dyn for<'js> FnOnce(Ctx<'js>) -> rquickjs::Result<()>>;

thread_local! {
    static ENGINE: OnceCell<&'static Engine> = const { OnceCell::new() };
    static TASKS: RefCell<Vec<Task>> = const { RefCell::new(Vec::new()) };
    /// Set while QuickJS starts its context (see `gettimeofday`).
    static STARTING: Cell<bool> = const { Cell::new(false) };
}

fn engine() -> Result<&'static Engine, String> {
    if let Some(js) = ENGINE.with(|e| e.get().copied()) {
        return Ok(js);
    }
    let js: &'static Engine = Box::leak(Box::new(Engine::new()?));
    ENGINE.with(|e| e.set(js)).ok();
    Ok(js)
}

impl Engine {
    fn new() -> Result<Self, String> {
        let rt = Runtime::new_with_alloc(Malloc).map_err(|e| e.to_string())?;
        STARTING.set(true);
        let ctx = Context::custom::<(
            Date,
            Eval,
            RegExpCompiler,
            RegExp,
            Json,
            Proxy,
            MapSet,
            TypedArrays,
            Promises,
            WeakRef,
        )>(&rt);
        STARTING.set(false);
        let ctx = ctx.map_err(|e| e.to_string())?;
        // atob and btoa (with DOMException), which rquickjs has no intrinsic for.
        unsafe { rquickjs::qjs::JS_AddIntrinsicAToB(ctx.as_raw().as_ptr()) };
        let (serve, answer) = ctx.with(|ctx| {
            let start = || {
                let host = Object::new(ctx.clone())?;
                host.set("kvGet", Function::new(ctx.clone(), kv_get)?)?;
                host.set("kvSet", Function::new(ctx.clone(), kv_set)?)?;
                host.set("kvDelete", Function::new(ctx.clone(), kv_delete)?)?;
                host.set("kvList", Function::new(ctx.clone(), kv_list)?)?;
                host.set("configAll", Function::new(ctx.clone(), config_all)?)?;
                host.set("random", Function::new(ctx.clone(), random_bytes)?)?;
                host.set("mathRandom", Function::new(ctx.clone(), math_random)?)?;
                host.set("encode", Function::new(ctx.clone(), encode)?)?;
                host.set("decode", Function::new(ctx.clone(), decode)?)?;
                host.set("url", Function::new(ctx.clone(), parse_url)?)?;
                host.set("urlSet", Function::new(ctx.clone(), set_url)?)?;
                host.set("print", Function::new(ctx.clone(), print)?)?;
                host.set("send", Function::new(ctx.clone(), send)?)?;
                host.set("sleep", Function::new(ctx.clone(), sleep)?)?;
                let bind: Function = ctx.eval::<Function, _>(PRELUDE)?.call((host,))?;
                let (module, evaluated) = Module::declare(ctx.clone(), "app.js", APP)?.eval()?;
                while ctx.execute_pending_job() {}
                match evaluated.result::<Value>() {
                    Some(evaluated) => drop(evaluated?),
                    None => {
                        return Err(Exception::throw_message(
                            &ctx,
                            "the module's top-level code must not wait for I/O",
                        ));
                    }
                }
                let List((serve, answer)): List<(Function, Function)> =
                    bind.call((module.get::<_, Value>("default")?,))?;
                Ok((Persistent::save(&ctx, serve), Persistent::save(&ctx, answer)))
            };
            start().map_err(|e| thrown(&ctx, e))
        })?;
        Ok(Self { rt, ctx, serve, answer })
    }

    async fn serve(
        &'static self,
        request: Request,
    ) -> Result<(u16, Vec<String>, Vec<u8>, Option<Persistent<Value<'static>>>), String> {
        let (response, js_ctx) = self.ctx.with(|ctx| {
            let call = || {
                let headers: Vec<&str> = request.headers().iter().flat_map(|(k, v)| [k.as_str(), v.as_str()]).collect();
                let body = match request.body() {
                    [] => Value::new_null(ctx.clone()),
                    _ => TypedArray::<u8>::new(ctx.clone(), request.body())?.into_value(),
                };
                let args = (nanovm_sdk::id(), request.method(), request.url(), headers, body);
                let List((response, js_ctx)): List<(Value, Value)> = self.serve.clone().restore(&ctx)?.call(args)?;
                Ok((Persistent::save(&ctx, response), Persistent::save(&ctx, js_ctx)))
            };
            call().map_err(|e| thrown(&ctx, e))
        })?;
        let response = self.settle(response).await?;
        self.ctx.with(|ctx| {
            let read = || {
                let args = (response.restore(&ctx)?, js_ctx.restore(&ctx)?);
                let answer: Array = self.answer.clone().restore(&ctx)?.call(args)?;
                let body: Option<TypedArray<u8>> = answer.get(2)?;
                let after: Option<Value> = answer.get(3)?;
                Ok((
                    answer.get(0)?,
                    answer.get(1)?,
                    body.map_or(Vec::new(), |b| bytes(&b)),
                    after.map(|p| Persistent::save(&ctx, p)),
                ))
            };
            read().map_err(|e| thrown(&ctx, e))
        })
    }

    /// Runs the engine's jobs until `value`, if it is a promise, settles
    /// (waiting on the host whenever only host calls and timers are left):
    /// its value.
    async fn settle(&self, value: Persistent<Value<'static>>) -> Result<Persistent<Value<'static>>, String> {
        loop {
            loop {
                match self.rt.execute_pending_job() {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(job) => return Err(job.0.with(|ctx| exception(&ctx.catch()))),
                }
            }
            let settled = self.ctx.with(|ctx| {
                let value = value.clone().restore(&ctx).map_err(|e| e.to_string())?;
                let Some(promise) = value.as_promise() else {
                    return Ok(Some(Persistent::save(&ctx, value)));
                };
                match promise.result::<Value>() {
                    None => Ok(None),
                    Some(Ok(v)) => Ok(Some(Persistent::save(&ctx, v))),
                    Some(Err(e)) => Err(thrown(&ctx, e)),
                }
            })?;
            if let Some(v) = settled {
                return Ok(v);
            }
            let done = poll_fn(|cx| {
                TASKS.with_borrow_mut(|tasks| {
                    let mut done = Vec::new();
                    tasks.retain_mut(|t| match t.as_mut().poll(cx) {
                        Poll::Ready(settle) => {
                            done.push(settle);
                            false
                        }
                        Poll::Pending => true,
                    });
                    match (done.is_empty(), tasks.is_empty()) {
                        (true, true) => Poll::Ready(None),
                        (true, false) => Poll::Pending,
                        (false, _) => Poll::Ready(Some(done)),
                    }
                })
            });
            let done = done.await.ok_or("a promise the request waits on can never settle")?;
            self.ctx
                .with(|ctx| done.into_iter().try_for_each(|settle| settle(ctx.clone())).map_err(|e| thrown(&ctx, e)))?;
        }
    }
}

fn spawn(task: impl Future<Output = Settle> + 'static) {
    TASKS.with_borrow_mut(|tasks| tasks.push(Box::pin(task)));
}

/// What the engine threw, as text.
fn thrown(ctx: &Ctx, e: rquickjs::Error) -> String {
    if e.is_exception() { exception(&ctx.catch()) } else { e.to_string() }
}

/// A JavaScript exception as `String(e)`, with its stack if it has one.
fn exception(e: &Value) -> String {
    let text = e.get::<Coerced<String>>().map_or_else(|_| "(an exception that can't be shown)".into(), |s| s.0);
    match e.as_object().and_then(|o| o.get::<_, Option<String>>("stack").ok().flatten()) {
        Some(stack) if !stack.is_empty() => format!("{text}\n{}", stack.trim_end()),
        _ => text,
    }
}

/// A `Uint8Array`'s bytes.
fn bytes(a: &TypedArray<u8>) -> Vec<u8> {
    // Safety: copied before any JavaScript runs.
    unsafe { a.as_bytes() }.unwrap_or_default().to_vec()
}

/// QuickJS's memory: libc's malloc, as for the rest of the runtime, but
/// running out traps (as in Rust code, so the object's memory limit kills
/// the instance) instead of becoming an exception the app could catch.
struct Malloc;

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn calloc(count: usize, size: usize) -> *mut c_void;
    fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
    fn malloc_usable_size(ptr: *mut c_void) -> usize;
}

fn or_trap(ptr: *mut c_void) -> *mut u8 {
    if ptr.is_null() {
        std::process::abort();
    }
    ptr.cast()
}

unsafe impl rquickjs::allocator::Allocator for Malloc {
    fn alloc(&mut self, size: usize) -> *mut u8 {
        or_trap(unsafe { malloc(size) })
    }
    fn calloc(&mut self, count: usize, size: usize) -> *mut u8 {
        or_trap(unsafe { calloc(count, size) })
    }
    unsafe fn dealloc(&mut self, ptr: *mut u8) {
        unsafe { free(ptr.cast()) }
    }
    // QuickJS never asks for 0 bytes (it frees instead).
    unsafe fn realloc(&mut self, ptr: *mut u8, size: usize) -> *mut u8 {
        or_trap(unsafe { realloc(ptr.cast(), size) })
    }
    unsafe fn usable_size(ptr: *mut u8) -> usize {
        unsafe { malloc_usable_size(ptr.cast()) }
    }
}

/// QuickJS seeds Math.random and its Map hashes from the clock when it
/// starts a context, which pre-initialization does at build time, where
/// the clock must not be read and the seed would be frozen in the snapshot:
/// so the clock reads 0 then. Math.random is replaced (`math_random`); the
/// Map seed is xored onto finished hashes, so it never separated keys that
/// collide. Every other read (`Date`) is the host's clock.
#[unsafe(no_mangle)]
unsafe extern "C" fn gettimeofday(tv: *mut [i64; 2], _: *mut u8) -> i32 {
    let now = if STARTING.get() {
        [0, 0]
    } else {
        let t = system_clock::now();
        [t.seconds, i64::from(t.nanoseconds / 1000)]
    };
    unsafe { tv.write(now) };
    0
}

// ---- host functions --------------------------------------------------------

thread_local! {
    /// `Math.random`'s state, seeded at first use (0: not yet).
    static SEED: Cell<u64> = const { Cell::new(0) };
}

/// A key-value call (the SDK's, on the composed provider) as a promise of
/// what `then` makes of its answer; an error rejects it. The store is opened
/// at first use, at run time: no handle may be in the snapshot.
fn kv_call<'js, T: 'static>(
    ctx: &Ctx<'js>,
    call: impl Future<Output = Result<T, kv::Error>> + 'static,
    then: impl for<'a> FnOnce(Ctx<'a>, T) -> rquickjs::Result<Value<'a>> + 'static,
) -> rquickjs::Result<Promise<'js>> {
    let (promise, resolve, reject) = Promise::new(ctx)?;
    let (resolve, reject) = (Persistent::save(ctx, resolve), Persistent::save(ctx, reject));
    spawn(async move {
        let answer = call.await;
        let settle: Settle = Box::new(move |ctx| match answer {
            Ok(v) => resolve.restore(&ctx)?.call((then(ctx.clone(), v)?,)),
            Err(e) => reject.restore(&ctx)?.call((Exception::from_message(ctx.clone(), &format!("kv: {e:?}"))?,)),
        });
        settle
    });
    Ok(promise)
}

/// `kvGet(key)`: a promise of the value stored at `key` (as JSON), or undefined.
fn kv_get<'js>(ctx: Ctx<'js>, key: String) -> rquickjs::Result<Promise<'js>> {
    kv_call(&ctx, async move { kv::get(&key).await }, |ctx, value| match value {
        Some(json) => ctx.json_parse(json),
        None => Ok(Value::new_undefined(ctx)),
    })
}

/// `kvSet(key, value)`: stores `value` as JSON.
fn kv_set<'js>(ctx: Ctx<'js>, key: String, value: Value<'js>) -> rquickjs::Result<Promise<'js>> {
    let json = ctx.json_stringify(value)?;
    let json =
        json.ok_or_else(|| Exception::throw_type(&ctx, "kv.put: the value can't be stored as JSON"))?.to_string()?;
    kv_call(&ctx, async move { kv::set(&key, json).await }, |ctx, ()| Ok(Value::new_undefined(ctx)))
}

fn kv_delete(ctx: Ctx, key: String) -> rquickjs::Result<Promise> {
    kv_call(&ctx, async move { kv::delete(&key).await }, |ctx, ()| Ok(Value::new_undefined(ctx)))
}

/// `kvList()`: a promise of every key.
fn kv_list(ctx: Ctx) -> rquickjs::Result<Promise> {
    kv_call(&ctx, kv::keys(), |ctx, keys| keys.into_js(&ctx))
}

fn config_all(ctx: Ctx) -> rquickjs::Result<Vec<String>> {
    let all = nanovm_sdk::config::get_all().map_err(|e| Exception::throw_message(&ctx, &format!("config: {e:?}")))?;
    Ok(all.into_iter().flat_map(|(k, v)| [k, v]).collect())
}

fn random_bytes(ctx: Ctx, n: u32) -> rquickjs::Result<TypedArray<u8>> {
    TypedArray::new(ctx, random::get_random_bytes(n.into()))
}

/// `Math.random`: xorshift64*, seeded from the host at first use.
fn math_random() -> f64 {
    let mut x = match SEED.get() {
        0 => random::get_random_u64() | 1,
        x => x,
    };
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    SEED.set(x);
    (x.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64 / (1u64 << 53) as f64
}

/// `TextEncoder.encode` (the prelude passes well-formed strings).
fn encode(ctx: Ctx, s: String) -> rquickjs::Result<TypedArray<u8>> {
    TypedArray::new(ctx, s.into_bytes())
}

/// `TextDecoder.decode`: UTF-8, invalid bytes replaced unless `fatal`.
fn decode<'js>(
    ctx: Ctx<'js>,
    b: TypedArray<'js, u8>,
    fatal: bool,
    ignore_bom: bool,
) -> rquickjs::Result<rquickjs::String<'js>> {
    // Safety: no JavaScript runs while `b` is borrowed.
    let b = unsafe { b.as_bytes() }.unwrap_or_default();
    let b = if ignore_bom { b } else { b.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(b) };
    match std::str::from_utf8(b) {
        Ok(s) => rquickjs::String::from_str(ctx, s),
        Err(_) if fatal => Err(Exception::throw_type(&ctx, "TextDecoder: the data is not valid UTF-8")),
        Err(_) => rquickjs::String::from_str(ctx.clone(), &String::from_utf8_lossy(b)),
    }
}

/// `new URL(input, base)`, by the WHATWG parser of the url crate: its parts,
/// `[href, origin, protocol, username, password, host, hostname, port,
/// pathname, search, hash]`.
fn parse_url(ctx: Ctx, input: String, base: Option<String>) -> rquickjs::Result<Vec<String>> {
    let url = match &base {
        Some(base) => Url::parse(base).and_then(|base| base.join(&input)),
        None => Url::parse(&input),
    };
    url.map(|u| parts(&u)).map_err(|e| Exception::throw_type(&ctx, &format!("Invalid URL {input:?}: {e}")))
}

/// A URL setter: the parts of `href` with `part` set to `value`. Like the
/// browser's, setters other than `href` ignore values they can't take.
fn set_url(ctx: Ctx, href: String, part: String, value: String) -> rquickjs::Result<Vec<String>> {
    let mut url = Url::parse(&href).map_err(|e| Exception::throw_type(&ctx, &e.to_string()))?;
    let _ = match part.as_str() {
        "href" => {
            quirks::set_href(&mut url, &value)
                .map_err(|e| Exception::throw_type(&ctx, &format!("Invalid URL {value:?}: {e}")))?;
            Ok(())
        }
        "protocol" => quirks::set_protocol(&mut url, &value),
        "username" => quirks::set_username(&mut url, &value),
        "password" => quirks::set_password(&mut url, &value),
        "host" => quirks::set_host(&mut url, &value),
        "hostname" => quirks::set_hostname(&mut url, &value),
        "port" => quirks::set_port(&mut url, &value),
        "pathname" => Ok(quirks::set_pathname(&mut url, &value)),
        "search" => Ok(quirks::set_search(&mut url, &value)),
        "hash" => Ok(quirks::set_hash(&mut url, &value)),
        _ => Ok(()),
    };
    Ok(parts(&url))
}

fn parts(u: &Url) -> Vec<String> {
    use quirks::*;
    let parts =
        [href(u), &origin(u), protocol(u), username(u), password(u), host(u), hostname(u), port(u), pathname(u)];
    parts.into_iter().chain([search(u), hash(u)]).map(String::from).collect()
}

/// `console`: `fd` 1 is the object's stdout, 2 its stderr.
fn print(fd: u32, text: String) {
    if fd == 2 { eprintln!("{text}") } else { println!("{text}") }
}

/// `send(method, url, headers, body)`: an outgoing request through the host
/// (so under the app's egress policy). Resolves to `[status, headers, body]`,
/// or to the error's text.
fn send<'js>(
    ctx: Ctx<'js>,
    method: String,
    url: String,
    headers: Vec<String>,
    body: Option<TypedArray<'js, u8>>,
) -> rquickjs::Result<Promise<'js>> {
    let mut request = nanovm_sdk::fetch(url.clone()).method(&method);
    for kv in headers.chunks_exact(2) {
        request = request.header(&kv[0], &kv[1]);
    }
    if let Some(body) = body {
        request = request.body(bytes(&body));
    }
    let (promise, resolve, _) = Promise::new(&ctx)?;
    let resolve = Persistent::save(&ctx, resolve);
    spawn(async move {
        let reply = request.send().await;
        let settle: Settle = Box::new(move |ctx| {
            let resolve = resolve.restore(&ctx)?;
            match reply {
                Ok(r) => {
                    let headers: Vec<String> = r.headers.into_iter().flat_map(|(k, v)| [k, v]).collect();
                    resolve.call((List((r.status, headers, TypedArray::new(ctx.clone(), r.body)?)),))
                }
                Err(e) => resolve.call((format!("fetch {url}: {e:?}"),)),
            }
        });
        settle
    });
    Ok(promise)
}

/// `sleep(ms, then)`: calls `then` after `ms` milliseconds (timers).
fn sleep<'js>(ctx: Ctx<'js>, ms: f64, then: Function<'js>) {
    let then = Persistent::save(&ctx, then);
    spawn(async move {
        monotonic_clock::wait_for((ms.max(0.0) * 1e6) as u64).await;
        let settle: Settle = Box::new(move |ctx| then.restore(&ctx)?.call(()));
        settle
    });
}
