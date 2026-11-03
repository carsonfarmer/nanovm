# W-18: synchronous KV over P3 filesystem

This is an ABI feasibility probe, **not a replacement KV provider**. Env and
config remain host-injected. No production provider, root WIT, or `guests/**`
source is changed. The separate workspace is not published by guest CI.

## Reproduce

Requires Rust's `wasm32-wasip2` target and `wac-cli 0.12.0` (`cargo install
wac-cli --version 0.12.0 --locked`). From the repository root:

```sh
bash experiments/kv-filesystem/build.sh
ulimit -n 100000
env -u RUST_BACKTRACE cargo run --locked -p nanovm --example kv_probe
```

The build composes an async HTTP client with a provider exporting the unchanged
`wasi:keyvalue/store@0.2.0-draft` resource interface. The provider uses P3 filesystem
imports through `guests/util`, not native KV or a new host backchannel. Its get/set
attempt real I/O at `.kv/default/<base64url-key>`. Other methods deliberately
return unsupported errors; this is not full KV conformance. Set uses the helper's
truncate/write semantics for the ABI test, not native KV replacement semantics.

The runner instantiates this component through the real `Node` with disposable
storage and asserts:

1. `/control` returns a provider-only error marker, proving composition selected
   the Wasm provider rather than native KV. Resource construction works on the
   subsequent calls.
2. `/direct` writes and reads `filesystem-control` through P3 filesystem imports
   from the async HTTP handler; this commits the initial file.
3. `/read` and `/write` both trap with **cannot block a synchronous task before
   returning** when synchronous KV needs P3 I/O to complete.
4. After the next instance opens the volume and recovers its journal, the original
   committed file remains intact. Error headers alone do not mean recovery ran.

All four assertions passed on Wasmtime 50 after merging SQLite main `491e790`.
The full locked nanovm suite passed: 107 unit and 67 integration tests, with ten
integration tests ignored (including optional SQLite fixtures). This does not
claim the optional SQLite suite ran.
Strict release clippy for the experiment passes. Replacing get's real I/O with
an in-memory control value makes the probe fail at `/read` (HTTP 200 rather than
the required suspension trap). Restored with `git checkout --` on the provider,
confirmed an empty diff, rebuilt and reran successfully.
This demonstrates the blocking boundary, not full quota/rollback/descriptor
conformance, production jail behavior, or performance. No runtime code is removed.

## Interpretation

The restriction is the **synchronous component function type**, not Rust syntax.
An async outer HTTP export and `block_on` inside the provider do not change it.
Immediate async completion can work; arbitrary external I/O cannot rely on that.
The [canonical ABI](https://github.com/WebAssembly/component-model/blob/38db7eeef95130ad87770454e97885f6db401db9/design/mvp/CanonicalABI.md)
permits the async canonical option only for async-typed functions. Wasmtime 50
enforces this with `CannotBlockSyncTask`. Upstream wasi-keyvalue's current
`0.2.0-draft2` still declares synchronous methods; it is not an async alternative.

Supported next experiments:

- **Async KV contract:** a distinctly named experimental interface can compose
  over P3 filesystem, but migrating existing KV imports is a product decision.
- **Host KV over shared filesystem operations:** preserve the existing standard
  interface while making quota/journal mutation ownership common. This supports
  one persistence path without requiring every adapter to run inside Wasm.

A private preloaded KV cache is not an equivalent implementation: direct file
writes and KV must see each other immediately, open descriptors must retain their
semantics, and both commit boundaries must govern one working state. Building a
unified in-memory filesystem is a separate project, not an ABI workaround.

**Successor prompt:** Read W-18 and Q73. Reproduce this probe before changing the
KV implementation. Keep env/config host-injected and guests unchanged. Decide
with the owner whether to prototype a distinctly named async KV contract or
centralize native KV mutations in the filesystem owner. Do not silently change
standard WIT or claim a full KV replacement from this ABI probe.
