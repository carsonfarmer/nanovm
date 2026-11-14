//! Pre-initializes a JavaScript app's component (js/build.sh runs it): calls
//! its `wizer-initialize` export, which starts the engine and evaluates the
//! app, and writes out the component with the memory that left, so a cold
//! start begins there. Every import but std's hash seed traps, so nothing
//! the host made (a handle, an environment, a clock) can end up in the
//! snapshot.
//!
//!     cargo run --release --example js-wizer -- in.wasm out.wasm

use wasmtime::component::{ComponentType, Instance, Lift, Linker, TypedFunc, WasmList};
use wasmtime::{Config, Engine, Store};
use wasmtime_wizer::{ComponentInstanceState, Wizer};

struct State<'a> {
    store: &'a mut Store<()>,
    instance: Instance,
}

impl State<'_> {
    fn func<R: ComponentType + Lift + Send + Sync + 'static>(&mut self, instance: &str, func: &str) -> TypedFunc<(), (R,)> {
        let (_, i) = self.instance.get_export(&mut *self.store, None, instance).unwrap();
        let (_, f) = self.instance.get_export(&mut *self.store, Some(&i), func).unwrap();
        self.instance.get_typed_func(&mut *self.store, f).unwrap()
    }

    async fn call<R: ComponentType + Lift + Send + Sync + 'static>(&mut self, instance: &str, func: &str) -> R {
        let f = self.func::<R>(instance, func);
        f.call_async(&mut *self.store, ()).await.unwrap().0
    }
}

impl ComponentInstanceState for State<'_> {
    async fn call_func_ret_list_u8(&mut self, instance: &str, func: &str, contents: impl FnOnce(&[u8]) + Send) {
        let list: WasmList<u8> = self.call(instance, func).await;
        contents(list.as_le_slice(&*self.store));
    }
    async fn call_func_ret_s32(&mut self, instance: &str, func: &str) -> i32 {
        self.call(instance, func).await
    }
    async fn call_func_ret_s64(&mut self, instance: &str, func: &str) -> i64 {
        self.call(instance, func).await
    }
    async fn call_func_ret_f32(&mut self, instance: &str, func: &str) -> u32 {
        self.call::<f32>(instance, func).await.to_bits()
    }
    async fn call_func_ret_f64(&mut self, instance: &str, func: &str) -> u64 {
        self.call::<f64>(instance, func).await.to_bits()
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [input, output] = &args[..] else { anyhow::bail!("usage: js-wizer in.wasm out.wasm") };
    let mut wizer = Wizer::new();
    // Dropping it leaves the component aliasing a core export that is gone.
    wizer.keep_init_func(true);
    let wasm = std::fs::read(input)?;
    let (cx, instrumented) = wizer.instrument_component(&wasm)?;
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;
    let component = wasmtime::component::Component::new(&engine, &instrumented)?;
    let mut linker = Linker::new(&engine);
    // Rust's std seeds its hash maps' keys on first use, at start-up. They
    // end up in the published component either way, so a constant keeps
    // builds reproducible.
    for (name, _) in component.component_type().imports(&engine) {
        if name.starts_with("wasi:random/insecure-seed@") {
            linker.instance(name)?.func_wrap("insecure-seed", |_, (): ()| Ok(((0x6e616e6f_766d6a73u64, 0u64),)))?;
        }
    }
    linker.define_unknown_imports_as_traps(&component)?;
    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let init = instance.get_typed_func::<(), (Result<(), String>,)>(&mut store, "wizer-initialize")?;
    let (evaluated,) = init.call_async(&mut store, ()).await.map_err(|e| {
        anyhow::Error::from(e).context(
            "evaluating the app at build time: top-level code must not reach the host \
             (fetch, env, Math.random, crypto, Date.now); do that in fetch()",
        )
    })?;
    evaluated.map_err(|e| anyhow::anyhow!("the app failed to evaluate: {e}"))?;
    let snapshot = wizer.snapshot_component(&cx, &mut State { store: &mut store, instance }).await?;
    std::fs::write(output, snapshot)?;
    Ok(())
}
