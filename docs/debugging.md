# Debugging an app

## 1. Read what the node said

When nanovm itself answers (the guest trapped, ran out of a limit, or never
started), the response has an `x-nanovm-error` header and a JSON body
`{"error": "<reason>", "detail": "<why>"}`. [ops/errors.md](ops/errors.md)
(branch `ideas/errors-guide`) lists every reason, what causes it and what to
do; this page is about the guest's own bugs.

For a trap the detail is the trap and the last 2 KiB the guest wrote to stdout
and stderr:

```
$ curl -s http://hostile.localhost:8443/x/panic          # tools/local-stack
{"detail":"trap: wasm trap: wasm `unreachable` instruction executed\nthread '<unnamed>' (1)
 panicked at hostile/src/lib.rs:112:21:\nhostile guest panicked on purpose\n...","error":"trap"}
```

A Rust panic names its file and line even in a stripped release build (the
location is data, not debug info), so for a panic that line is usually enough.
A trap without a panic (an out-of-bounds access, a stack overflow, an integer
divide by zero, C's `abort`) shows only the trap's kind. The same detail is in
the worker's `trap` event, and everything the guest printed is exported as
OpenTelemetry log records (`log.iostream` = `stdout`/`stderr`) with the
request's trace id.

`RUST_BACKTRACE` doesn't help: inside a guest it prints `stack backtrace:` and
no frames (wasm can't walk its own stack), and set on the node it costs most of
its throughput (README, Host requirements).

## 2. Get the backtrace: `tools/debug-run`

The node does not report a trap's wasm backtrace. `tools/debug-run` runs one
request through the component outside the node, with the imports the node
links, and prints the guest's output as it happens and, on a trap, every frame:

```
$ tools/debug-run hostile-debug.wasm GET /x/panic     # built as below
thread '<unnamed>' (1) panicked at hostile/src/lib.rs:112:21:
hostile guest panicked on purpose
debug-run: wasm trap: wasm `unreachable` instruction executed (the node answers 500 trap, ...)
error while executing at wasm backtrace:
    ...
   10:  0x323a3 - core[cff34068e63f4426]::panicking::panic_fmt
                    at /rustc/58cf8f9c.../library/core/src/panicking.rs:80:14
   11:  0x3f152 - {async_fn#0}
                    at /home/user/wt/tooling/guests/hostile/src/lib.rs:112:21
   12:  0x3d4ef - {async_fn#0}
                    at /home/user/wt/tooling/guests/hostile/src/lib.rs:32:30
```

Options: `--body TEXT`, `--body-file F`, `--config K=V` (as `wasi:config`
reads it), `--env K=V`, `--memory-mb N` (64), and `--data DIR`: `/data` starts
as a copy of `DIR`, so a bug that depends on an object's data can be replayed
on a copy of it (export it with `tools/volume.py export`, then `tar -xf` and use
its `data/`). Nothing is written back. Outbound HTTP is refused, as with an
empty egress list. There is no CPU budget or deadline, and the filesystem is the
stock one, not the node's quota'd, journaled one: a quota or rollback bug won't
reproduce here, a logic bug will.

Frames have names only if the component keeps its `name` section, and files and
lines only with DWARF. The test guests strip both (`strip = true`), so their
frames read `<unknown>!<wasm function 563>`. For a build to debug:

```toml
[profile.release]
debug = "line-tables-only"   # file:line for each frame
strip = false                # function names
```

or without editing anything,
`CARGO_PROFILE_RELEASE_DEBUG=line-tables-only CARGO_PROFILE_RELEASE_STRIP=none cargo build --release ...`,
then compose with `wac plug` as usual. Measured on the hostile guest: the
composed component grows from 439 KB to 1,357 KB, and compiling it costs the
same CPU (0.77-0.84 s, three runs each). Such a build also runs unchanged on a
node, which never reads the DWARF: its workers start with an empty environment
(so `WASMTIME_BACKTRACE_DETAILS` doesn't reach them), and it doesn't report
backtraces anyway. DWARF records the build machine's paths; add
`--remap-path-prefix` to `RUSTFLAGS` before publishing one.

## 3. What the node's wasmtime leaves out

The node builds wasmtime without guest debugging (gdb/lldb through
`debug-builtins`), core dumps, profiler support (perf maps, jitdump, VTune) and
the text format (`crates/nanovm/Cargo.toml`). So `perf` on a worker shows guest
code as unknown JIT frames, and there are no core dumps. For a debugger or a
profile of guest code, run the component under the `wasmtime` CLI, which has
these (`-g`, `-D coredump=...`, `--profile`); that was not tried here with
nanovm's WASIp3 guests.

## 4. Before it runs

- `tools/inspect app.wasm --config nanovm.json [--node nanovm.toml]`: the imports
  the node refuses, whether its engine and linker accept the component, what it
  is composed of, and the limits after the node clamps them, including a
  `memory_mb` below the component's starting memory (every start would fail
  with `instantiate-failed`).
- `tools/local-stack`: the app behind the real proxy, DNS record and jailed
  worker, redeployed when its `.wasm` changes.

## 5. Editors

A guest is a `wasm32-wasip2` crate; tell rust-analyzer so it checks the code
the guest compiles (`.vscode/settings.json` in the guest's workspace):

```json
{ "rust-analyzer.cargo.target": "wasm32-wasip2" }
```

Plain logic (parsing, routing) can still have native unit tests: keep it in
functions that don't touch `wasip3` and `cargo test` them on the host.
