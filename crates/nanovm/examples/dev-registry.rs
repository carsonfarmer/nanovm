//! A registry on loopback for local development: publishes each guest in
//! `guests/` (its component and its `nanovm.json`), prints each as a
//! `nanovm-app` names it, then serves until killed. A node in dev mode runs
//! them unsigned.
//!
//!     scripts/build-guests.sh
//!     cargo run --example dev-registry [-- 127.0.0.1:5000]

use std::path::Path;

use anyhow::Context;
use nanovm::registry::{APP_TYPE, CONFIG_TYPE, Oci, VERSION, WASM_TYPE};
use oci_client::client::{ClientConfig, ClientProtocol, Config, ImageLayer};
use oci_client::manifest::{OCI_IMAGE_MEDIA_TYPE, OciImageManifest};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr = std::env::args().nth(1).unwrap_or("127.0.0.1:5000".into()).parse()?;
    let mut registry = container_registry::ContainerRegistry::builder().build_for_testing();
    registry.bind(addr);
    let running = registry.run_in_background();
    let guests = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../guests");
    let client = oci_client::Client::new(ClientConfig {
        protocol: ClientProtocol::Http,
        ..Default::default()
    });
    for guest in ["counter", "hostile"] {
        let wasm = guests.join(format!("target/wasm32-wasip2/release/{guest}.wasm"));
        let wasm = std::fs::read(&wasm).with_context(|| format!("{}: run scripts/build-guests.sh", wasm.display()))?;
        let spec = std::fs::read(guests.join(guest).join("nanovm.json"))?;
        let (layer, config) = (ImageLayer::new(wasm, WASM_TYPE.into(), None), Config::new(spec, CONFIG_TYPE.into(), None));
        let version = [(VERSION.to_string(), "1.0.0".to_string())].into();
        let mut manifest = OciImageManifest::build(std::slice::from_ref(&layer), &config, Some(version));
        (manifest.media_type, manifest.artifact_type) = (Some(OCI_IMAGE_MEDIA_TYPE.into()), Some(APP_TYPE.into()));
        let r = format!("{}/nanovm/{guest}:latest", running.bound_addr()).parse()?;
        let auth = oci_client::secrets::RegistryAuth::Anonymous;
        client.push(&r, &[layer], config, &auth, Some(manifest)).await?;
        let digest = Oci::new(true)?.digest(&r).await?;
        println!("{guest} {}/{}@{digest}", r.registry(), r.repository());
    }
    std::future::pending().await
}
