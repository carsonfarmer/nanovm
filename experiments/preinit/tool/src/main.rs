//! Pre-initializes a component (the K-100 spike; experiments/preinit/README.md):
//! calls its `wizer-initialize` export once at build time and writes out the
//! component with the linear memories and globals that left, so every
//! instance starts there.
//!
//!     preinit in.wasm out.wasm [--init EXPORT] [--seed]
//!
//! Every import traps while the initializer runs, so nothing the host could
//! hand it (a file under /data, a clock, the environment, a handle) can end
//! up in the snapshot; an initializer that reaches for one fails the build.
//! `--seed` answers `wasi:random/insecure-seed` with a constant instead, for
//! code that builds a std HashMap at start: its keys are then the same in
//! every instance (see the README). Derived from js/'s js-wizer (cf/onramp).

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
    let (mut files, mut init, mut seed) = (vec![], "wizer-initialize".to_string(), false);
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--init" => init = it.next().expect("--init EXPORT"),
            "--seed" => seed = true,
            _ => files.push(a),
        }
    }
    let [input, output] = &files[..] else { anyhow::bail!("usage: preinit in.wasm out.wasm [--init EXPORT] [--seed]") };
    let mut wizer = Wizer::new();
    wizer.init_func(init.clone());
    // Dropping it leaves the component aliasing a core export that is gone.
    wizer.keep_init_func(true);
    let wasm = std::fs::read(input)?;
    let (cx, instrumented) = wizer.instrument_component(&wasm)?;
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;
    let component = wasmtime::component::Component::new(&engine, &instrumented)?;
    let mut linker = Linker::new(&engine);
    if seed {
        for (name, _) in component.component_type().imports(&engine) {
            if name.starts_with("wasi:random/insecure-seed@") {
                linker.instance(name)?.func_wrap("insecure-seed", |_, (): ()| Ok(((0x6e616e6f_766d7069u64, 0u64),)))?;
            }
        }
    }
    linker.define_unknown_imports_as_traps(&component)?;
    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let f = instance.get_typed_func::<(), ()>(&mut store, init.as_str())?;
    f.call_async(&mut store, ()).await.map_err(|e| {
        anyhow::Error::from(e).context("the initializer failed: it must not reach the host (files, clocks, env, randomness, I/O)")
    })?;
    let snapshot = wizer.snapshot_component(&cx, &mut State { store: &mut store, instance }).await?;
    std::fs::write(output, &snapshot)?;
    eprintln!("preinit: {} bytes -> {} bytes", wasm.len(), snapshot.len());
    Ok(())
}
