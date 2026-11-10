# Developer and operator tools

None of these is part of the node: they use its library (Rust ones are
examples of the `nanovm` crate) or its on-disk layout (Python ones).

| Tool | What it does | Tests |
|---|---|---|
| `tools/inspect app.wasm [--config nanovm.json] [--node nanovm.toml]` | A component's imports (flags those the node refuses), exports, composition, whether the node's engine and linker accept it, and its limits after clamping. Exits 1 with the refusals. | `cargo test -p nanovm --example inspect` |
| `tools/volume.py list\|export\|import` | One object's `/data` as a TAR in local capture's format: offline under the app's storage lock, or a committed version while the node runs; imported into a fresh root. | `tools/test_volume.py`, `crates/nanovm/tests/volume.rs` |
| `tools/doctor [nanovm.toml]` | Checks this host for a node (fds, mappings, address space, Landlock, seccomp, capabilities, filesystem, cgroup delegation, clock) and says what to fix. Runs `deploy/single-node/check-host.py`. | `deploy/single-node/test_check_host.py` |
| `tools/local-stack [HOST=app.wasm[,nanovm.json] ...]` | Registry, stub DNS-over-HTTPS resolver, dev node and proxy on loopback; apps served at `http://<host>.localhost:8443/`, redeployed when their `.wasm` changes. | `crates/nanovm/tests/stack.rs` |
| `tools/debug-run app.wasm [METHOD] /object/path` | One request outside the node, with a trap's whole wasm backtrace ([docs/debugging.md](../docs/debugging.md)). | `cargo test -p nanovm --example debug-run` |

Measurement: `cargo run -p nanovm --example coldstart -- NAME=a.wasm ...`
compares components' compile, cached-load and first-request costs and their
private memory per object ([experiments/preinit](../experiments/preinit/README.md)).
