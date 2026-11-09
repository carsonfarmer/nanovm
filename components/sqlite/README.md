# SQLite provider (source beta)

SQLite runs inside a composed Wasm component over nanovm's existing Preview 3
filesystem. Apps use the vendored, unchanged `spin:sqlite/sqlite@3.1.0` async
interface. No SQLite host import, new journal format, or special filesystem is
required. This source beta is not a published package or a promise of full Spin
behavior. Use it with this branch's host recovery fixes, not an older nanovm.

## Build and run

Install Rust's `wasm32-wasip2` target, unpack
[WASI SDK 34](https://github.com/WebAssembly/wasi-sdk/releases/tag/wasi-sdk-34),
and install `wac-cli` 0.12.0. From the repository root:

```sh
rustup target add wasm32-wasip2
cargo install wac-cli --version 0.12.0 --locked
WASI_SDK_PATH=/path/to/wasi-sdk-34.0-x86_64-linux bash components/sqlite/build.sh
cargo run --locked -p nanovm --example sqlite
```

The separate Cargo workspace and checked-in lockfile pin the provider/example
dependencies; CI downloads WASI SDK 34 with SHA-256 verification. Build through
the script: its SQLite flags select the P3 VFS and memory-only temporary storage.
The script emits:

- `target/sqlite/provider.wasm`: the provider, without test hooks.
- `target/sqlite/example.wasm`: the HTTP app composed with that provider.

The [app](example/src/lib.rs) opens the `default` database, executes parameterized
SQL, drains the result stream, and awaits completion. `POST /set` accepts a UTF-8
body (up to 1 KiB) and writes the same value to SQL, a normal file and KV. `GET /`
returns all three values. `POST /set?rollback=1` intentionally returns a guest
error **after** all three writes, demonstrating whole-request rollback. Remove
that demonstration route in an actual app.

The [runner](../../crates/nanovm/examples/sqlite.rs) sends HTTP requests directly
through a real in-process Node, without a socket listener, OCI registry or jail.
It asserts exact values, object isolation, recovery after the intentional error,
and persistence after Node restart. It uses a fresh directory under `target/`,
removes it on success and retains it on failure. Expected final output:

```text
PASS: composed SQLite persistence, object isolation, mixed SQL/file/KV rollback, restart
```

For your own app, import `spin:sqlite/sqlite@3.1.0`, use `open-async` and
`execute-async`, then compose the provider before publishing your ordinary app
artifact through nanovm's existing signed OCI workflow:

```sh
wac plug path/to/app.wasm --plug target/sqlite/provider.wasm -o target/app-with-sqlite.wasm
```

The uncomposed app is intentionally rejected by the host's import allowlist.
The normal provider artifact remains separate from fault-injection builds.

## Supported contract

- **One provider instance and one live connection per object.** Open the name
  `default`; another name or concurrent open is rejected. The example opens and
  drops a connection per request for clarity. Caching across successful requests
  is supported; a failed request discards the whole instance before recovery.
- **No app-facing journaling controls.** Internally DELETE/FULL. Transaction
  control, savepoints, ATTACH/DETACH and PRAGMA controls are denied (the
  read-only `integrity_check`, `data_version` and `page_size` are allowed:
  FTS5, FTS3/4 and R-Tree ask the last two themselves). SQL statements finish inside the call; there
  are no open SQL transactions or live cursors spanning responses.
- **Request atomicity includes SQL, files and KV.** A successful HTTP response,
  including HTTP 4xx/5xx, commits. A guest error or trap rolls back. Do not catch
  an unsafe failure and return an HTTP error expecting rollback. Storage errors,
  SQLite interruption, and uncertain write-result failures trap deliberately.
- **Bounded materialized results:** 4,096 rows, with a shared 1 MiB budget across
  outstanding results (not an exact RSS limit). Drain the stream and await its
  completion future. Read-only overflow is catchable; write-result overflow
  aborts the request because the statement may already have modified data.
- **Cooperative database ownership.** Never access `/data/composed.db` or its
  sidecars through raw filesystem APIs or another provider. Other files and KV
  share the volume safely. Composition does not isolate these files from a
  malicious component holding the same filesystem capability. Locks are local
  to this sole owner, not inter-connection or inter-process locks.
- **Memory-only temporary SQL storage.** Sorts and temporary tables consume
  guest memory; filesystem spill is not supported. Configure memory/disk/CPU
  limits for your workload; the example's limits are illustrative.
- **Cancellation:** request CPU/time interruption and process death recover the
  object; abandoning results releases their budget. Individual query
  cancellation while continuing the request is not implemented. Execution
  currently does not yield to another app call; changing this requires revisiting
  serialization, interrupted-write behavior and resource lifetimes.
- **Durability:** local storage must honor file/directory sync. Keep volumes and
  their sibling journals on the same filesystem. Recovery failures stop the
  object and retain undo; repair the storage problem before retrying. Back up
  only quiescent, fully recovered objects. A failed commit or lost response can
  have an unknown outcome: design idempotent operations/retries. The example is
  a state replacement, not a general deduplication protocol.

## Validation and limits

CI builds this package, runs the example, then runs the existing SQL/file/KV
recovery, bounds, interruption and jailed-worker-crash tests against **this same
provider/VFS source**:

```sh
WASI_SDK_PATH=/path/to/wasi-sdk-34.0-x86_64-linux bash experiments/sqlite-filesystem/build.sh --compose
env -u RUST_BACKTRACE RUST_TEST_THREADS=1 cargo test --locked -p nanovm --test integration sqlite_ -- --ignored --nocapture
```

These tests remain opt-in locally because they require the C toolchain and Wasm
fixtures, but are mandatory in CI. The broader direct-linked mode/benchmark
experiments are not additional supported provider modes. See the
[investigation and evidence](../../experiments/sqlite-filesystem/README.md).
Physical power loss, media corruption and arbitrary host I/O faults are not
certified; guest traps and process kills are not substitutes for those tests.
