//! A registry on loopback for local development: publishes each guest in
//! `guests/` (its component and its `nanovm.json`), prints each as a
//! `nanovm-app` names it, then serves until killed. A node in dev mode runs
//! them unsigned.
//!
//!     scripts/build-guests.sh
//!     cargo run --example dev-registry [-- 127.0.0.1:5000]

use std::path::Path;

use anyhow::Context;
use nanovm::registry::Oci;
use oci_spec::distribution::Reference;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr = std::env::args().nth(1).unwrap_or("127.0.0.1:5000".into()).parse()?;
    let mut registry = container_registry::ContainerRegistry::builder().build_for_testing();
    registry.bind(addr);
    let running = registry.run_in_background();
    let guests = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../guests");
    let oci = Oci::new(true)?;
    for guest in ["counter", "hostile"] {
        let wasm = guests.join(format!("target/wasm32-wasip2/release/{guest}.wasm"));
        let wasm = std::fs::read(&wasm).with_context(|| format!("{}: run scripts/build-guests.sh", wasm.display()))?;
        let spec = std::fs::read(guests.join(guest).join("nanovm.json"))?;
        let r: Reference = format!("{}/nanovm/{guest}:latest", running.bound_addr()).parse()?;
        let digest = oci.push_app(&r, None, spec, wasm, &semver::Version::new(1, 0, 0)).await?;
        println!("{guest} {}/{}@{digest}", r.registry(), r.repository());
    }
    std::future::pending().await
}
