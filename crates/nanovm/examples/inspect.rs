//! What a node would make of a component, before it is published: its
//! imports (and which ones nanovm refuses), its exports, whether the node's
//! own engine and linker accept it, how it is composed, and the limits its
//! `nanovm.json` declares, as the node would clamp them.
//!
//!     tools/inspect app.wasm [--config nanovm.json] [--node nanovm.toml]
//!
//! The verdicts come from the runtime itself: `app::import_allowed` (the
//! allowlist the node checks at load), `engine::build` (the pooling
//! allocator's per-component limits), `app::linker` (the imports the node
//! provides, at the versions it provides them) and `registry::AppSpec` (how
//! the node parses `nanovm.json`). Exits 1 if the node would refuse the
//! component or its config.

use std::path::PathBuf;

use anyhow::{Context, bail};
use nanovm::config::{Limits, NodeConfig};
use wasmparser::{
    ComponentAlias, ComponentExternalKind, ComponentInstance, ComponentName, ComponentOuterAliasKind, ComponentTypeRef,
    Encoding, Instance, KnownCustom, Parser, Payload,
};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (mut wasm, mut spec, mut node) = (None, None, None);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" => spec = Some(PathBuf::from(args.next().context("--config needs a path")?)),
            "--node" => node = Some(PathBuf::from(args.next().context("--node needs a path")?)),
            _ if wasm.is_none() && !a.starts_with('-') => wasm = Some(PathBuf::from(a)),
            _ => bail!("usage: inspect <app.wasm> [--config nanovm.json] [--node nanovm.toml]"),
        }
    }
    let wasm = wasm.context("usage: inspect <app.wasm> [--config nanovm.json] [--node nanovm.toml]")?;
    let bytes = std::fs::read(&wasm).with_context(|| format!("reading {}", wasm.display()))?;
    let node = match &node {
        Some(p) => NodeConfig::load(p)?,
        None => NodeConfig::default(),
    };
    let refused = report(&wasm, &bytes, spec.as_deref(), &node)?;
    if !refused.is_empty() {
        println!("\nnanovm would refuse this:");
        for r in &refused {
            println!("  {r}");
        }
        std::process::exit(1);
    }
    println!("\nnanovm would load this");
    Ok(())
}

/// Prints the report; returns what the node would refuse.
fn report(
    path: &std::path::Path,
    bytes: &[u8],
    spec: Option<&std::path::Path>,
    node: &NodeConfig,
) -> anyhow::Result<Vec<String>> {
    use sha2::Digest;
    let mut refused = vec![];
    let root = walk(bytes).context("parsing the component")?;
    println!(
        "{}: {} bytes, sha256:{:x}",
        path.display(),
        bytes.len(),
        sha2::Sha256::digest(bytes)
    );

    // The node's own engine, sized for one object: per-component limits and
    // the memory reservation are the node's.
    let engine = nanovm::engine::build(&NodeConfig {
        max_live_objects: 1,
        ..node.clone()
    })?;
    let component = match wasmtime::component::Component::new(&engine, bytes) {
        Ok(c) => Some(c),
        Err(e) => {
            refused.push(format!("the node's engine refuses it: {e:#}"));
            println!("\nFAIL the node's engine refuses it: {e:#}");
            None
        }
    };

    println!("\nimports:");
    let imports: Vec<String> = match &component {
        Some(c) => c
            .component_type()
            .imports(&engine)
            .map(|(n, _)| n.to_string())
            .collect(),
        None => root.imports.iter().map(|(n, _)| n.clone()).collect(),
    };
    for name in &imports {
        if nanovm::app::import_allowed(name) {
            println!("  ok      {name}");
        } else {
            refused.push(format!("import {name} is not permitted"));
            println!("  REFUSED {name}");
        }
    }
    if imports.iter().any(|n| !nanovm::app::import_allowed(n)) {
        println!("  (a guest may import only {})", nanovm::app::ALLOWED_IMPORTS.join(" "));
        if imports
            .iter()
            .any(|n| n.starts_with("spin:key-value/") || n.starts_with("spin:sqlite/"))
        {
            println!("  (compose the key-value or SQLite provider first: components/keyvalue, components/sqlite)");
        }
    }
    println!("exports:");
    for (name, kind) in &root.exports {
        println!("  {name} ({kind})");
    }

    if let Some(c) = &component {
        let linked = nanovm::app::linker(&engine)?
            .instantiate_pre(c)
            .and_then(wasmtime_wasi_http::p3::bindings::ServicePre::new);
        match linked {
            Ok(_) => println!("links: ok (the node provides every import and finds wasi:http/handler)"),
            Err(e) => {
                refused.push(format!("linking: {e:#}"));
                println!("links: FAIL {e:#}");
            }
        }
    }

    println!("\ncomposition:");
    print_component(&root, &path.file_name().unwrap_or_default().to_string_lossy(), 1);
    let t = root.totals();
    println!(
        "when instantiated: {} core instances, {} memories starting at {:.2} MiB in all, {} tables",
        t.core_instances,
        t.memories,
        t.memory_bytes as f64 / (1 << 20) as f64,
        t.tables
    );

    // Limits: as the node parses the app's config and clamps it.
    let declared = match spec {
        Some(p) => {
            let text = std::fs::read(p).with_context(|| format!("reading {}", p.display()))?;
            match serde_json::from_slice::<nanovm::registry::AppSpec>(&text) {
                Ok(s) => {
                    println!(
                        "\n{}: env {:?}, config keys {:?}",
                        p.display(),
                        s.env.keys().collect::<Vec<_>>(),
                        s.config.keys().collect::<Vec<_>>()
                    );
                    Some(s.limits)
                }
                Err(e) => {
                    refused.push(format!("{}: {e}", p.display()));
                    println!("\nFAIL {}: the node cannot parse it: {e}", p.display());
                    None
                }
            }
        }
        None => {
            println!("\nlimits: no --config given; these are the defaults");
            Some(Limits::default())
        }
    };
    if let Some(declared) = declared {
        let effective = declared.clamped(&node.limits);
        let (d, m, e) = (
            serde_json::json!(declared),
            serde_json::json!(node.limits),
            serde_json::json!(effective),
        );
        println!(
            "  {:<26} {:>10} {:>10} {:>10}",
            "limit", "declared", "node max", "effective"
        );
        for (k, v) in e.as_object().expect("limits are a struct") {
            if k == "egress" {
                continue;
            }
            let mark = if d[k] != *v { "  (clamped)" } else { "" };
            println!(
                "  {k:<26} {:>10} {:>10} {:>10}{mark}",
                d[k].to_string(),
                m[k].to_string(),
                v.to_string()
            );
        }
        let egress = if effective.egress.is_empty() {
            "none (all outbound HTTP denied)".to_string()
        } else {
            effective.egress.join(", ")
        };
        println!("  egress: {egress}");
        let handles = effective
            .max_resources
            .min((effective.memory_mb << 20) as usize / nanovm::server::MAX_HEAD);
        println!("  handles per instance: {handles} (max_resources, and at most memory_mb x 16)");
        if t.memory_bytes > effective.memory_mb << 20 {
            let why = format!(
                "its memories start at {:.2} MiB, over memory_mb = {}: every instance would fail to start (instantiate-failed)",
                t.memory_bytes as f64 / (1 << 20) as f64,
                effective.memory_mb
            );
            println!("FAIL {why}");
            refused.push(why);
        }
    }
    Ok(refused)
}

// ---- the component's structure, from the binary -----------------------------

#[derive(Default)]
struct Module {
    name: Option<String>,
    size: usize,
    /// Initial bytes of each memory it defines.
    memories: Vec<u64>,
    tables: usize,
    data: usize,
    producers: Vec<String>,
}

#[derive(Default)]
struct Comp {
    name: Option<String>,
    size: usize,
    producers: Vec<String>,
    imports: Vec<(String, &'static str)>,
    exports: Vec<(String, &'static str)>,
    /// The component index space: `Some` for one defined here.
    components: Vec<Option<Comp>>,
    /// Names the component-name section gives nested components, by index.
    component_names: Vec<(u32, String)>,
    modules: Vec<Option<Module>>,
    instances: Vec<Inst>,
    core_instantiations: Vec<u32>,
}

enum Inst {
    Import(String),
    Of { component: u32, args: Vec<(String, u32)> },
    Export { instance: u32, name: String },
    Other,
}

#[derive(Default)]
struct Totals {
    core_instances: usize,
    memories: usize,
    memory_bytes: u64,
    tables: usize,
}

impl Comp {
    /// Nested component `i`, by its name if it has one, else by what it
    /// exports.
    fn component_name(&self, i: u32) -> String {
        let named = self
            .component_names
            .iter()
            .find(|(n, _)| *n == i)
            .map(|(_, s)| s.clone());
        let inner = self.components.get(i as usize).and_then(Option::as_ref);
        let name = named.or_else(|| inner?.name.clone()).or_else(|| {
            let exports: Vec<_> = inner?.exports.iter().map(|(n, _)| n.as_str()).collect();
            (!exports.is_empty()).then(|| format!("the one exporting {}", exports.join(", ")))
        });
        format!("[{i}] {}", name.unwrap_or_else(|| "component".into()))
    }

    /// Where instance `i`, passed as `arg`, comes from, in words.
    fn describe(&self, i: u32, arg: &str, depth: usize) -> String {
        match self.instances.get(i as usize) {
            _ if depth > 8 => format!("instance {i}"),
            Some(Inst::Import(n)) => format!("the host's {n}"),
            Some(Inst::Of { component, .. }) => format!("[{component}]"),
            Some(Inst::Export { instance, name }) if name == arg || name.is_empty() => {
                self.describe(*instance, arg, depth + 1)
            }
            Some(Inst::Export { instance, name }) => format!("{name} of {}", self.describe(*instance, name, depth + 1)),
            _ => format!("instance {i}"),
        }
    }

    /// What instantiating this component creates, as wasmtime's pooling
    /// allocator counts it (core instances and their defined memories and
    /// tables, through nested component instances).
    fn totals(&self) -> Totals {
        let mut t = Totals::default();
        for &m in &self.core_instantiations {
            t.core_instances += 1;
            if let Some(Some(m)) = self.modules.get(m as usize) {
                t.memories += m.memories.len();
                t.memory_bytes += m.memories.iter().sum::<u64>();
                t.tables += m.tables;
            }
        }
        for inst in &self.instances {
            if let Inst::Of { component, .. } = inst
                && let Some(Some(c)) = self.components.get(*component as usize)
            {
                let n = c.totals();
                t.core_instances += n.core_instances;
                t.memories += n.memories;
                t.memory_bytes += n.memory_bytes;
                t.tables += n.tables;
            }
        }
        t
    }
}

fn print_component(c: &Comp, label: &str, depth: usize) {
    let pad = "  ".repeat(depth);
    let producers = if c.producers.is_empty() {
        String::new()
    } else {
        format!(" [{}]", c.producers.join(", "))
    };
    println!("{pad}{label}: {} bytes{producers}", c.size);
    for (i, m) in c.modules.iter().enumerate() {
        if let Some(m) = m {
            let mems: Vec<String> = m.memories.iter().map(|b| format!("{} KiB", b >> 10)).collect();
            let producers = if m.producers.is_empty() {
                String::new()
            } else {
                format!(" [{}]", m.producers.join(", "))
            };
            println!(
                "{pad}  core module {i}{}: {} bytes, memory {}, {} table(s), {} bytes of data{producers}",
                m.name.as_ref().map(|n| format!(" {n}")).unwrap_or_default(),
                m.size,
                if mems.is_empty() {
                    "none of its own".into()
                } else {
                    mems.join(" + ")
                },
                m.tables,
                m.data
            );
        }
    }
    let shown = |i: u32| !c.component_name(i).ends_with("-shim-component");
    let nested = |i: u32| {
        c.components
            .get(i as usize)
            .and_then(Option::as_ref)
            .filter(|_| shown(i))
    };
    for inst in &c.instances {
        let Inst::Of { component, args } = inst else { continue };
        if nested(*component).is_none() {
            continue;
        }
        let wired: Vec<_> = args
            .iter()
            .filter(|(_, from)| !matches!(c.instances.get(*from as usize), Some(Inst::Import(_))))
            .collect();
        let host = args.len() - wired.len();
        println!(
            "{pad}  instantiates {}: {host} import(s) from the host",
            c.component_name(*component)
        );
        for (name, from) in wired {
            println!("{pad}    {name} <- {}", c.describe(*from, name, 0));
        }
    }
    for (i, n) in c.components.iter().enumerate() {
        if let Some(n) = n.as_ref().filter(|_| shown(i as u32)) {
            print_component(n, &c.component_name(i as u32), depth + 1);
        }
    }
}

fn kind(k: ComponentExternalKind) -> &'static str {
    match k {
        ComponentExternalKind::Module => "module",
        ComponentExternalKind::Func => "func",
        ComponentExternalKind::Value => "value",
        ComponentExternalKind::Type => "type",
        ComponentExternalKind::Instance => "instance",
        ComponentExternalKind::Component => "component",
    }
}

fn type_ref(t: &ComponentTypeRef) -> &'static str {
    match t {
        ComponentTypeRef::Module(_) => "module",
        ComponentTypeRef::Func(_) => "func",
        ComponentTypeRef::Value(_) => "value",
        ComponentTypeRef::Type(_) => "type",
        ComponentTypeRef::Instance(_) => "instance",
        ComponentTypeRef::Component(_) => "component",
    }
}

enum Frame {
    Comp(Comp),
    Module(Module),
}

fn producers(r: wasmparser::ProducersSectionReader) -> anyhow::Result<Vec<String>> {
    let mut out = vec![];
    for field in r {
        let field = field?;
        for v in field.values {
            let v = v?;
            out.push(if v.version.is_empty() {
                v.name.to_string()
            } else {
                format!("{} {}", v.name, v.version)
            });
        }
    }
    Ok(out)
}

/// The component's tree: nested components and modules, with the index
/// spaces the composition is wired through.
fn walk(bytes: &[u8]) -> anyhow::Result<Comp> {
    let mut stack: Vec<Frame> = vec![];
    let mut sizes: Vec<usize> = vec![bytes.len()];
    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload?;
        match payload {
            Payload::Version { encoding, .. } => {
                let size = sizes.pop().unwrap_or(0);
                stack.push(match encoding {
                    Encoding::Component => Frame::Comp(Comp {
                        size,
                        ..Comp::default()
                    }),
                    Encoding::Module => Frame::Module(Module {
                        size,
                        ..Module::default()
                    }),
                });
            }
            Payload::ModuleSection { unchecked_range, .. } | Payload::ComponentSection { unchecked_range, .. } => {
                sizes.push((unchecked_range.end - unchecked_range.start) as usize);
            }
            Payload::End(_) => match stack.pop() {
                Some(Frame::Comp(c)) => match stack.last_mut() {
                    Some(Frame::Comp(parent)) => parent.components.push(Some(c)),
                    Some(Frame::Module(_)) => bail!("a component inside a module"),
                    None => return Ok(c),
                },
                Some(Frame::Module(m)) => match stack.last_mut() {
                    Some(Frame::Comp(parent)) => parent.modules.push(Some(m)),
                    _ => bail!("not a component (a core module): nanovm runs components"),
                },
                None => bail!("unbalanced component"),
            },
            p => match stack.last_mut() {
                Some(Frame::Comp(c)) => component_section(c, p)?,
                Some(Frame::Module(m)) => module_section(m, p)?,
                None => bail!("no header"),
            },
        }
    }
    bail!("truncated component")
}

fn component_section(c: &mut Comp, p: Payload) -> anyhow::Result<()> {
    match p {
        Payload::ComponentImportSection(r) => {
            for i in r {
                let i = i?;
                let name = i.name.full_name().to_string();
                c.imports.push((name.clone(), type_ref(&i.ty)));
                match i.ty {
                    ComponentTypeRef::Instance(_) => c.instances.push(Inst::Import(name)),
                    ComponentTypeRef::Component(_) => c.components.push(None),
                    ComponentTypeRef::Module(_) => c.modules.push(None),
                    _ => {}
                }
            }
        }
        Payload::ComponentExportSection(r) => {
            for e in r {
                let e = e?;
                c.exports.push((e.name.full_name().to_string(), kind(e.kind)));
                // An export is a new index in its sort's space.
                match e.kind {
                    ComponentExternalKind::Instance => c.instances.push(Inst::Export {
                        instance: e.index,
                        name: String::new(),
                    }),
                    ComponentExternalKind::Component => c.components.push(None),
                    ComponentExternalKind::Module => c.modules.push(None),
                    _ => {}
                }
            }
        }
        Payload::ComponentInstanceSection(r) => {
            for i in r {
                c.instances.push(match i? {
                    ComponentInstance::Instantiate { component_index, args } => Inst::Of {
                        component: component_index,
                        args: args
                            .iter()
                            .filter(|a| a.kind == ComponentExternalKind::Instance)
                            .map(|a| (a.name.to_string(), a.index))
                            .collect(),
                    },
                    ComponentInstance::FromExports(_) => Inst::Other,
                });
            }
        }
        Payload::InstanceSection(r) => {
            for i in r {
                if let Instance::Instantiate { module_index, .. } = i? {
                    c.core_instantiations.push(module_index);
                }
            }
        }
        Payload::ComponentAliasSection(r) => {
            for a in r {
                match a? {
                    ComponentAlias::InstanceExport {
                        kind: ComponentExternalKind::Instance,
                        instance_index,
                        name,
                    } => c.instances.push(Inst::Export {
                        instance: instance_index,
                        name: name.to_string(),
                    }),
                    ComponentAlias::InstanceExport {
                        kind: ComponentExternalKind::Component,
                        ..
                    } => c.components.push(None),
                    ComponentAlias::InstanceExport {
                        kind: ComponentExternalKind::Module,
                        ..
                    } => c.modules.push(None),
                    ComponentAlias::Outer {
                        kind: ComponentOuterAliasKind::Component,
                        ..
                    } => c.components.push(None),
                    ComponentAlias::Outer {
                        kind: ComponentOuterAliasKind::CoreModule,
                        ..
                    } => c.modules.push(None),
                    _ => {}
                }
            }
        }
        Payload::CustomSection(s) => match s.as_known() {
            KnownCustom::ComponentName(r) => {
                for n in r {
                    match n? {
                        ComponentName::Component { name, .. } => c.name = Some(name.to_string()),
                        ComponentName::Components(map) => {
                            for n in map {
                                let n = n?;
                                c.component_names.push((n.index, n.name.to_string()));
                            }
                        }
                        _ => {}
                    }
                }
            }
            KnownCustom::Producers(r) => c.producers = producers(r)?,
            _ => {}
        },
        _ => {}
    }
    Ok(())
}

fn module_section(m: &mut Module, p: Payload) -> anyhow::Result<()> {
    match p {
        Payload::MemorySection(r) => {
            for mem in r {
                let mem = mem?;
                m.memories.push(mem.initial << mem.page_size_log2.unwrap_or(16));
            }
        }
        Payload::TableSection(r) => m.tables += r.count() as usize,
        Payload::DataSection(r) => {
            for d in r {
                m.data += d?.data.len();
            }
        }
        Payload::CustomSection(s) => match s.as_known() {
            KnownCustom::Name(r) => {
                for n in r {
                    if let wasmparser::Name::Module { name, .. } = n? {
                        m.name = Some(name.to_string());
                    }
                }
            }
            KnownCustom::Producers(r) => m.producers = producers(r)?,
            _ => {}
        },
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn repo(p: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(p)
    }

    fn inspect(wasm: &str, spec: Option<&str>) -> Vec<String> {
        let path = repo(wasm);
        let bytes = std::fs::read(&path).unwrap_or_else(|_| panic!("{}: run scripts/build-guests.sh", path.display()));
        let spec = spec.map(|s| {
            let p = std::env::temp_dir().join(format!("inspect-test-{}-{}.json", std::process::id(), s.len()));
            std::fs::write(&p, s).unwrap();
            p
        });
        report(&path, &bytes, spec.as_deref(), &NodeConfig::default()).unwrap()
    }

    /// The composed test guest loads; the same guest before composition
    /// imports key-value, which the node refuses, as it does at load.
    #[test]
    fn refuses_what_the_node_refuses() {
        let counter = std::fs::read_to_string(repo("guests/counter/nanovm.json")).unwrap();
        assert_eq!(
            inspect("guests/target/wasm32-wasip2/release/counter.wasm", Some(&counter)),
            Vec::<String>::new()
        );
        let refused = inspect("target/guests/wasm32-wasip2/release/counter.wasm", None);
        assert!(
            refused[0].contains("import spin:key-value/key-value@3.0.0 is not permitted"),
            "{refused:?}"
        );
        assert!(refused[1].starts_with("linking:"), "{refused:?}");
        assert_eq!(refused.len(), 2, "{refused:?}");
    }

    /// Limits as the node reads them: an unknown key is an error, and a
    /// memory limit under the component's starting memory fails every start.
    #[test]
    fn checks_the_declared_limits() {
        let wasm = "guests/target/wasm32-wasip2/release/counter.wasm";
        let refused = inspect(wasm, Some(r#"{"limits": {"memory": 16}}"#));
        assert!(
            refused.len() == 1 && refused[0].contains("unknown field `memory`"),
            "{refused:?}"
        );
        let refused = inspect(wasm, Some(r#"{"limits": {"memory_mb": 2}}"#));
        assert!(
            refused.len() == 1 && refused[0].contains("over memory_mb = 2"),
            "{refused:?}"
        );
        // The node's engine: memories reserved smaller than the guest's.
        let node = NodeConfig {
            memory_reservation_mb: 1,
            ..NodeConfig::default()
        };
        let refused = report(&repo(wasm), &std::fs::read(repo(wasm)).unwrap(), None, &node).unwrap();
        assert!(
            refused.len() == 1 && refused[0].contains("exceeds the limit of 0x100000 bytes"),
            "{refused:?}"
        );
    }

    /// The composition walk finds the provider wired into the app.
    #[test]
    fn walks_the_composition() {
        let root = walk(&std::fs::read(repo("guests/target/wasm32-wasip2/release/counter.wasm")).unwrap()).unwrap();
        let t = root.totals();
        assert_eq!((t.memories, t.core_instances), (2, 6));
        let wired: Vec<_> = root
            .instances
            .iter()
            .filter_map(|i| match i {
                Inst::Of { args, .. } => Some(
                    args.iter()
                        .filter(|(_, from)| !matches!(root.instances[*from as usize], Inst::Import(_)))
                        .map(|(name, from)| format!("{name} <- {}", root.describe(*from, name, 0)))
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(wired, ["spin:key-value/key-value@3.0.0 <- [0]"]);
        assert_eq!(
            root.component_name(0),
            "[0] the one exporting spin:key-value/key-value@3.0.0"
        );
    }
}
