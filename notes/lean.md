# lean: a smaller, clearer runtime

Branch `cf/lean`, from `491e790`. The goal was to make the runtime
(`crates/nanovm/src`) smaller and clearer without changing what it does,
and to build fewer dependencies. Every change below keeps the test suite
green (see "Tests") and changes no limit, error code or telemetry field.

## Results, before → after

| | before (`491e790`) | after (`cf/lean`) | method |
|---|---|---|---|
| Distinct crates in a normal build | 322 | 265 (−57, −18%) | `cargo tree -p nanovm -e normal --prefix none`, `(*)` marks removed, `sort -u` |
| The brief's dependency count | 421 | 355 | same command with `(*)` lines kept, `sort -u \| wc -l` |
| C libraries built from source | includes zstd-sys and ittapi-sys | those two gone | the build's `Compiling` lines |
| Release binary, unstripped | 53,797,504 B | 48,240,912 B (−5.56 MB, −10.3%) | `cargo build --locked --release -p nanovm --bin nanovm` |
| Release binary, stripped | 37,258,848 B | 33,356,976 B (−3.90 MB, −10.5%) | `strip` of the same |
| Runtime lines (CI's count) | 5,346 | 5,351 (+5) | the python snippet in `.github/workflows/ci.yml` |

The line count is flat because dropping `oci-spec` cost 40 lines of our
own reference parser; the other simplifications removed 35. Without that
trade the runtime would be 35 lines smaller and still carry 21 more
crates, a regex engine and six proc-macro crates.

## What changed, and why it is safe

1. **Only the dependency features the node uses** (`868e006`).
   wasmtime was built with all its defaults, including things the node
   never configures: a second compile cache (the node keeps its own
   `.cwasm` cache in `engine.rs`), profilers (VTune, perf jitdump), core
   dumps, the text format (WAT), guest debugging, compile-time builtins
   and runtime WIT parsing. wasmtime-wasi also built WASIp1 (for core
   modules; the node runs components through p2/p3 only), wasmtime-wasi-http
   built `wasi:http@0.2` (only 0.3 is linked), and sigstore-trust-root
   built a TUF client (the node uses only the embedded trusted root).
   Everything that affects guests or error text stays: the accepted wasm
   proposals (`gc`, `threads`, `stack-switching`), backtraces with
   demangled names and `WASMTIME_BACKTRACE_DETAILS` support, parallel
   compilation and pooling. The engine's compatibility hash is unchanged,
   so cached `.cwasm` files stay valid. One input is now refused instead
   of parsed: a component in WAT text form, which was never a supported
   app format (the OCI layer is `application/wasm`); this also takes the
   WAT parser out of the jailed worker. Effect: 322 → 286 crates, and two
   C library builds (zstd, ittapi) gone.
2. **Our own OCI reference parser instead of `oci-spec`** (`d485694`).
   The runtime used `oci-spec` only to split `registry/repository:tag@digest`.
   Its parser is one large regex, slow enough that a per-thread cache of
   parsed references had been added to the request path; it also pulls
   in `regex` and six proc-macro crates. The new parser follows the same
   grammar with plain string splits, so the cache is gone too. A unit
   test compares it with `oci-spec` (still available to tests through
   `oci-client`) on oci-spec's own examples, edge cases and 100,000
   random strings built from reference pieces, more than 10,000 of them
   valid: the same strings are accepted, with the same parts. Four
   deliberate bugs in the parser each make that test fail. Two
   differences are on purpose: a tag must be ASCII, as the OCI spec says
   (oci-spec took any Unicode letter; no registry serves such a tag, and
   the proxy answered 502 either way; ISSUES N-90), and every malformed
   reference reports "invalid reference format". 286 → 265 crates;
   +40 runtime lines.
3. **Journal bookkeeping as gaps** (`eac8d42`). Before a write changes a
   file, the journal saves the original bytes it has not saved yet. That
   was a loop with a peekable iterator and three exits, followed after
   every record by sorting and rebuilding the whole list of saved
   ranges. Now it computes the unsaved gaps in one pass and inserts each
   saved piece next to its neighbours. Same records, same order, same
   chunks; a new test checks the merging, and three deliberate bugs each
   fail a test. −9 lines.
4. **Key-value errors through wasmtime's own mechanism** (`cee06aa`).
   kv.rs had a home-grown adapter (`done`) to turn "error the guest sees"
   versus "trap" into wasmtime's nested result type. wasmtime's bindgen
   does exactly that with `trappable_error_type` and one `convert_error`
   function. Same errors reach the guest; a stale handle still traps.
   −8 lines.
5. **Needless generality** (`29c8ddd`): a closure that fixed one path in
   config loading (`Path::join` already keeps absolute paths); an
   `rss_bytes` branch for platforms the crate cannot build on; a String
   copy of every telemetry event's detail; a wrapper closure in the
   proxy's publish listener; and a hand-written undo of the socket count
   when the proxy refuses a socket, which the connection's `Drop`
   already does (a test catches a miscount). −18 lines.
6. **A test that collided with other proxies** (`dd2efa4`): one proxy test
   left its publish port at the default 8444 and failed whenever anything
   else on the host used it (ISSUES N-92). Shown failing with 8444 held
   before the fix, passing after.

## Dependencies looked at and kept

`cargo tree` shows what removing each would actually save. Most are
already pulled in by wasmtime, reqwest or the OpenTelemetry SDK, so
dropping our direct use saves no crate:

- `async-trait` (needed by `opentelemetry_http::HttpClient`, and in
  wasmtime), `scopeguard` (in lock_api and encoding_rs; the owner chose it
  in Q69), `base64` (0.22 is used by other crates too), `semver` (in
  wasmtime), `rustix` (in wasmtime; its safe `SCM_RIGHTS` API keeps
  `link.rs` free of unsafe cmsg code), `opentelemetry-http` (in
  opentelemetry-otlp), `hyper-util` (in reqwest; its `client-legacy`
  feature is the proxy's node client), `futures` (its executor is also
  needed by opentelemetry_sdk): 0 crates each.
- `toml`: needed for the config format. With wasmtime's cache gone it is
  now the only `toml` in the build (there were two versions).
- `parking_lot` together with tokio's `full`: 3 crates, but it changes
  every lock in files other branches own (ISSUES W-40).
- `tracing-subscriber`: 4 crates, but replacing it means about 20 lines
  of our own subscriber to print SDK warnings to stderr, and a different
  stderr format. Not worth it.
- reqwest's platform verifier: 3 crates if the node's own TLS clients
  used the same compiled-in roots as guest egress; that is a trust
  decision for the owner (ISSUES W-42).
- wasmtime's `gc`, `threads`, `stack-switching`: these decide which
  guests load, so they stay until the owner decides (ISSUES W-43).

## Build time

Not measured as a clean build: the machine ran at load average 13 to 20
all day (other agents compiling) and the disk was nearly full, so two
clean release builds (one per tree) would neither have been reliable nor
fitted. What can be said from the changes themselves: 57 fewer crates
to compile, among them two C library builds (zstd-sys, ittapi-sys), six
proc-macro crates (derive_builder, darling, strum, getset, const_format,
wiggle's macros) and the regex engine. To measure on an idle machine:
`git archive <commit> | tar -x -C <dir>` for `491e790` and this branch's
head, then in each `CARGO_TARGET_DIR=<fresh> cargo build --offline
--locked --release -p nanovm --bin nanovm` under `time`, comparing user+sys
CPU seconds and the count of `Compiling` lines.

## Tests

- Full suite on the final state (`cargo test --locked -p nanovm`,
  `RUST_TEST_THREADS=1`) at load average 13 to 17 from other agents'
  builds: 109 unit tests pass; integration 65 pass, 10 ignored (as on
  the baseline), 2 failed and both are load flakes:
  `cpu_hogs_leave_cores_for_short_requests` (known load-sensitive,
  HANDOFF) passes alone, and `a_node_runs_at_most_max_apps` fails alone
  on the unchanged baseline too at this load (ISSUES N-91). Other
  single failures seen during the day, each passing alone or explained:
  `start_up_is_charged_guest_cpu_not_host_work` and
  `kv_reads_and_batches_are_bounded` (CPU budgets under load, N-32),
  `the_proxy_outlasts_idle_and_excess_connections` (N-92, fixed).
- Production check without the `dev` feature passes
  (`cargo check --locked -p nanovm --bin nanovm`).
- Mutation checks: see each change above.

## For the orchestrator

- Docs to fold in: PLAN.md "Phase 7" says "The runtime now depends on
  `oci-spec` for reference parsing"; it now parses references itself
  (`registry::Reference`), and oci-spec is only a test dependency through
  oci-client. HANDOFF's runtime line count (5,346 → 5,351 here).
- Environment: every worktree's `target/*/.cargo-*lock` files were one
  hard-linked inode, so all agents' builds serialized (reported and fixed
  during the run). `git stash` is shared by all worktrees; this branch
  left one entry, `WIP on cf/lean: 868e006` (its content is committed).
- `cf/lean` touches `registry.rs` and `proxy.rs` (sec's files this
  round) only where they used oci-spec's `Reference`, plus `publish`'s
  signature; `ws.rs` in `open`'s count check; `examples/dev-registry.rs`
  and one integration test helper for the new `Reference` type.
- Ideas for after the other branches land: ISSUES W-40 to W-43.
