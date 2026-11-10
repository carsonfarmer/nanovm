//! Runs one request through a component outside the node, to see why it
//! fails: the guest's stdout and stderr as they happen, and for a trap the
//! whole wasm backtrace, which the node does not report (its
//! `x-nanovm-error: trap` detail has the trap and the guest's last 2 KiB of
//! output). Function names need the component's `name` section (don't strip
//! it); file and line need DWARF (`debug = "line-tables-only"`). See
//! docs/debugging.md.
//!
//!     tools/debug-run app.wasm [METHOD] /object/path [--body TEXT | --body-file F]
//!         [--data DIR] [--config K=V]... [--env K=V]... [--memory-mb N]
//!
//! It links what the node links (WASIp2 io/clocks/cli/random, WASIp3 cli,
//! clocks, random and filesystem, wasi:config, wasi:http), with the stock
//! filesystem instead of the node's quota'd, journaled one, and refuses all
//! outbound HTTP. `/data` starts as a copy of `--data DIR` (say an object's
//! volume, from `tools/volume.py export` and `tar -x`), or empty; nothing is
//! written back. No CPU budget or deadline, so a debugger or `perf` can
//! take its time.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_config::{WasiConfig, WasiConfigVariables};
use wasmtime_wasi_http::{Error, RequestOptions, WasiBody, WasiHttpCtx, WasiHttpCtxView, WasiHttpHooks, WasiHttpView};

struct State {
    wasi: WasiCtx,
    table: ResourceTable,
    http: WasiHttpCtx,
    hooks: NoEgress,
    config: WasiConfigVariables,
    limits: StoreLimits,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for State {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.hooks,
        }
    }
}

/// Every outbound request is refused, as with an empty egress allowlist.
struct NoEgress;

type Sent = Box<
    dyn Future<
            Output = Result<
                (
                    http::Response<WasiBody>,
                    Box<dyn Future<Output = Result<(), Error>> + Send>,
                ),
                Error,
            >,
        > + Send,
>;

impl WasiHttpHooks for NoEgress {
    fn send_request(
        &mut self,
        request: http::Request<WasiBody>,
        _: Option<RequestOptions>,
        _: Box<dyn Future<Output = Result<(), Error>> + Send>,
    ) -> Sent {
        eprintln!("debug-run: refused outbound {} {}", request.method(), request.uri());
        Box::new(async { Err(Error::HttpRequestDenied) })
    }
}

type Answer = (http::StatusCode, http::HeaderMap, Bytes);
type ErrorCode = wasmtime_wasi_http::p3::bindings::http::types::ErrorCode;

struct Args {
    wasm: PathBuf,
    method: http::Method,
    path: String,
    body: Vec<u8>,
    data: Option<PathBuf>,
    config: Vec<(String, String)>,
    env: Vec<(String, String)>,
    memory_mb: usize,
}

const USAGE: &str = "usage: debug-run app.wasm [METHOD] /object/path [--body TEXT | --body-file F] [--data DIR] \
                     [--config K=V]... [--env K=V]... [--memory-mb N]";

fn args() -> anyhow::Result<Args> {
    let mut positional = vec![];
    let mut a = Args {
        wasm: PathBuf::new(),
        method: http::Method::GET,
        path: String::new(),
        body: vec![],
        data: None,
        config: vec![],
        env: vec![],
        memory_mb: 64,
    };
    let pair = |s: String| -> anyhow::Result<(String, String)> {
        let (k, v) = s.split_once('=').context(USAGE)?;
        Ok((k.into(), v.into()))
    };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        match x.as_str() {
            "--body" => a.body = it.next().context(USAGE)?.into_bytes(),
            "--body-file" => a.body = std::fs::read(it.next().context(USAGE)?)?,
            "--data" => a.data = Some(it.next().context(USAGE)?.into()),
            "--config" => a.config.push(pair(it.next().context(USAGE)?)?),
            "--env" => a.env.push(pair(it.next().context(USAGE)?)?),
            "--memory-mb" => a.memory_mb = it.next().context(USAGE)?.parse()?,
            _ if x.starts_with("--") => bail!("{USAGE}"),
            _ => positional.push(x),
        }
    }
    match &positional[..] {
        [wasm, path] => (a.wasm, a.path) = (wasm.into(), path.clone()),
        [wasm, method, path] => (a.wasm, a.method, a.path) = (wasm.into(), method.parse()?, path.clone()),
        _ => bail!("{USAGE}"),
    }
    Ok(a)
}

/// `src`'s files and directories, copied under `dst`.
fn copy_tree(src: &Path, dst: &Path) -> anyhow::Result<()> {
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let to = dst.join(e.file_name());
        if e.file_type()?.is_dir() {
            std::fs::create_dir(&to)?;
            copy_tree(&e.path(), &to)?;
        } else {
            std::fs::copy(e.path(), &to)?;
        }
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    match run(&args()?).await? {
        Ok(Ok((status, headers, body))) => {
            println!("{} {}", status.as_u16(), status.canonical_reason().unwrap_or(""));
            for (k, v) in &headers {
                println!("{k}: {}", v.to_str().unwrap_or("<binary>"));
            }
            println!();
            println!("{}", String::from_utf8_lossy(&body));
            Ok(())
        }
        Ok(Err(code)) => bail!("the handler returned an error code (the node answers 500 guest-error): {code:?}"),
        Err(e) => {
            let why = e
                .downcast_ref::<wasmtime::Trap>()
                .map_or_else(|| format!("{e:#}"), |t| t.to_string());
            eprintln!("debug-run: {why} (the node answers 500 trap, or memory past --memory-mb)");
            if let Some(trace) = e.downcast_ref::<wasmtime::WasmBacktrace>() {
                eprintln!("{trace}");
            }
            std::process::exit(2)
        }
    }
}

/// Runs the request: an answer, the handler's error code, or how the
/// instance failed (a trap, with its wasm backtrace attached).
async fn run(a: &Args) -> anyhow::Result<wasmtime::Result<Result<Answer, ErrorCode>>> {
    // The object id is the path's first segment, as the node takes it.
    let (object, rest) = a
        .path
        .trim_start_matches('/')
        .split_once('/')
        .unwrap_or((a.path.trim_start_matches('/'), ""));
    anyhow::ensure!(
        nanovm::table::valid_name(object),
        "{:?} does not start with an object id: /<object>/<path>",
        a.path
    );

    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.max_wasm_stack(512 << 10);
    config.async_stack_size(1 << 20);
    // File and line for each frame, from DWARF if the component has it.
    config.wasm_backtrace_details(wasmtime::WasmBacktraceDetails::Enable);
    let engine = Engine::new(&config)?;
    let bytes = std::fs::read(&a.wasm).with_context(|| format!("reading {}", a.wasm.display()))?;
    let component = Component::new(&engine, &bytes)?;

    let mut linker = Linker::<State>::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi::p3::cli::add_to_linker(&mut linker)?;
    wasmtime_wasi::p3::clocks::add_to_linker(&mut linker)?;
    wasmtime_wasi::p3::random::add_to_linker(&mut linker)?;
    wasmtime_wasi::p3::filesystem::add_to_linker(&mut linker)?;
    wasmtime_wasi_config::add_to_linker(&mut linker, |s: &mut State| WasiConfig::from(&s.config))?;
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    let pre = wasmtime_wasi_http::p3::bindings::ServicePre::new(linker.instantiate_pre(&component)?)?;

    static RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let scratch = std::env::temp_dir().join(format!("debug-run-{}-{n}", std::process::id()));
    let (data, tmp) = (scratch.join("data"), scratch.join("tmp"));
    std::fs::create_dir_all(&data)?;
    std::fs::create_dir_all(&tmp)?;
    let _cleanup = scopeguard::guard(scratch.clone(), |s| _ = std::fs::remove_dir_all(s));
    if let Some(src) = &a.data {
        copy_tree(src, &data).with_context(|| format!("copying {} to /data", src.display()))?;
    }
    let mut wasi = WasiCtxBuilder::new();
    wasi.inherit_stdout().inherit_stderr();
    wasi.env("NANOVM_APP", "debug-run").env("NANOVM_OBJECT_ID", object);
    for (k, v) in &a.env {
        wasi.env(k, v);
    }
    wasi.preopened_dir(&data, "/data", FsPerms::ReadWrite)?;
    wasi.preopened_dir(&tmp, "/tmp", FsPerms::ReadWrite)?;
    let state = State {
        wasi: wasi.build(),
        table: ResourceTable::new(),
        http: WasiHttpCtx::new(),
        hooks: NoEgress,
        config: a.config.iter().cloned().collect(),
        limits: StoreLimitsBuilder::new().memory_size(a.memory_mb << 20).build(),
    };
    let mut store = Store::new(&engine, state);
    store.limiter(|s| &mut s.limits);

    let req = http::Request::builder()
        .method(a.method.clone())
        .uri(format!("http://localhost/{rest}"))
        .header("host", "localhost")
        .body(Full::new(Bytes::from(a.body.clone())))?;
    let svc = pre
        .instantiate_async(&mut store)
        .await
        .map_err(anyhow::Error::from)
        .context("instantiating")?;
    Ok(store
        .run_concurrent(async move |acc| -> wasmtime::Result<Result<Answer, ErrorCode>> {
            let (req, io) = acc.with(|mut s| wasmtime_wasi_http::p3::Request::from_http(&mut s.data_mut().hooks, req));
            match svc.handle(acc, req).await? {
                Ok(res) => {
                    let res = acc.with(|mut s| res.into_http(&mut s, io))?;
                    let (parts, body) = res.into_parts();
                    let body = body
                        .collect()
                        .await
                        .map_err(|e| wasmtime::Error::msg(format!("{e:?}")))?
                        .to_bytes();
                    Ok(Ok((parts.status, parts.headers, body)))
                }
                Err(code) => Ok(Err(code)),
            }
        })
        .await
        .and_then(|r| r))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guest(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../guests/target/wasm32-wasip2/release")
            .join(format!("{name}.wasm"))
    }

    fn request(wasm: PathBuf, method: http::Method, path: &str) -> Args {
        Args {
            wasm,
            method,
            path: path.into(),
            body: vec![],
            data: None,
            config: vec![],
            env: vec![],
            memory_mb: 64,
        }
    }

    /// A panic is reported as the node can't: the trap and its frames.
    #[tokio::test]
    async fn a_trap_comes_with_its_backtrace() {
        let e = run(&request(guest("hostile"), http::Method::GET, "/x/panic"))
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(
            e.downcast_ref::<wasmtime::Trap>(),
            Some(&wasmtime::Trap::UnreachableCodeReached)
        );
        let frames = e.downcast_ref::<wasmtime::WasmBacktrace>().unwrap().frames().len();
        assert!(frames > 5, "{frames} frames");
    }

    /// `/data` starts as a copy of `--data`, which stays as it was; config
    /// and egress are as the node gives them.
    #[tokio::test]
    async fn runs_on_a_copy_of_a_volume() {
        let dir = std::env::temp_dir().join(format!("debug-run-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("count"), "41").unwrap();
        let mut a = request(guest("counter"), http::Method::POST, "/alice/incr");
        a.data = Some(dir.clone());
        let (status, _, body) = run(&a).await.unwrap().unwrap().unwrap();
        assert_eq!(
            (status.as_u16(), String::from_utf8_lossy(&body).contains("\"count\":42")),
            (200, true),
            "{body:?}"
        );
        assert_eq!(std::fs::read_to_string(dir.join("count")).unwrap(), "41");
        let mut a = request(guest("counter"), http::Method::GET, "/alice/config/greeting");
        a.config.push(("greeting".into(), "hi".into()));
        assert_eq!(run(&a).await.unwrap().unwrap().unwrap().2, "hi");
        let a = request(guest("hostile"), http::Method::GET, "/x/fetch?url=http://example.com/");
        let body = run(&a).await.unwrap().unwrap().unwrap().2;
        assert!(String::from_utf8_lossy(&body).contains("HttpRequestDenied"), "{body:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
