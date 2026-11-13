//! Every config file the repository ships loads with the loader that reads it
//! in production: the deploy references, the demo, and the examples' and
//! guests' manifests. A renamed or removed key then fails here, not on an
//! operator's host.

use std::path::{Path, PathBuf};

use nanovm::config::{NodeConfig, ProxyConfig};
use nanovm::registry::AppSpec;

/// Every file under `dir`, but build output and git's.
fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        match path.file_name().and_then(|n| n.to_str()) {
            Some("target" | ".git" | "node_modules") => {}
            _ if path.is_dir() => files(&path, out),
            _ => out.push(path),
        }
    }
}

/// `nanovm serve` reads a node's `*.toml` with `NodeConfig::load`, `nanovm
/// proxy` a `proxy.toml` with `ProxyConfig::load`, and `nanovm publish` and
/// `nanovm run` an app's `nanovm.json` as an `AppSpec`.
#[test]
fn every_shipped_config_loads() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let mut all = Vec::new();
    files(&root, &mut all);
    all.sort();
    let (mut loaded, mut failed) = (Vec::new(), Vec::new());
    for path in &all {
        let name = path.file_name().unwrap().to_str().unwrap();
        let result = match name {
            // Other tools' files.
            "Cargo.toml" | "rustfmt.toml" => continue,
            "proxy.toml" => ProxyConfig::load(path).map(drop),
            _ if name.ends_with(".toml") => NodeConfig::load(path).map(drop),
            "nanovm.json" => serde_json::from_slice::<AppSpec>(&std::fs::read(path).unwrap())
                .map(drop)
                .map_err(Into::into),
            _ => continue,
        };
        let shown = path.strip_prefix(&root).unwrap().display().to_string();
        match result {
            Ok(()) => loaded.push(shown),
            Err(e) => failed.push(format!("{shown}: {e:#}")),
        }
    }
    println!("loaded: {loaded:#?}");
    assert!(
        failed.is_empty(),
        "shipped configs that don't load:\n{}",
        failed.join("\n")
    );
    // The walk found what it is for.
    for expected in [
        "deploy/systemd/nanovm.toml",
        "deploy/systemd/proxy.toml",
        "demo/nanovm.toml",
        "guests/counter/nanovm.json",
    ] {
        assert!(
            loaded.iter().any(|f| f == expected),
            "{expected} not found among {loaded:?}"
        );
    }
}
