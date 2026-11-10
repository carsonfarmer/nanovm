# Pre-initializing Rust guests and the SQLite provider (K-100)

A research spike: report only, no runtime change. Question: does running a
component's start-up work once at build time (Wizer, here `wasmtime-wizer`
50.0.1 as a library) make cold starts cheaper for Rust guests and for the
composed SQLite provider, as it does for JavaScript apps (`js/` on
`cf/onramp`), and is it correct under nanovm?

**Answer.** It pays only for a guest that builds state at start: an app that
builds a 50,000-entry table on first use spent 12.9 ms of CPU on each new
object's first request, and 0.35 ms pre-initialized (36× less), and each live
object held 3,200 KiB of private memory instead of 37 KiB (the table is now
part of the shared, copy-on-write memory image). A Rust guest without such
state gains nothing (321 → 312 µs, inside the noise), and neither does the
SQLite provider: what it can do at build time (`sqlite3_initialize`) is a
small part of a first request that opens or creates its database under
`/data`, which must happen at run time. The costs: the component grows by the
state it carries (166 KB → 3.05 MB here), its memory starts that much bigger
(1.06 → 4.12 MiB, against `memory_mb`), loading it from the compile cache
takes about 2 ms more, and **every instance starts with the same std hash
keys** (below). Nothing from `/data` can get into a snapshot: every import
traps while the initializer runs, and one that reads `/data` fails the build.

Recommendation: don't add a pre-initialization step to the Rust SDK or to the
SQLite provider. Document it as an option for apps with heavy start-up state,
with the hash-key caveat; the JavaScript runtime keeps its own (3.7× there).

## What was measured

`cargo run -p nanovm --example coldstart -- --rounds 5 --objects 300 NAME=a.wasm …`
(`crates/nanovm/examples/coldstart.rs`): an in-process node per component
and round, the components' order reversed every other round. Per component:

- **App cold start:** `Node::new` with an empty compile cache (Cranelift
  compiles it) and again with the cache (deserializes it): process CPU.
- **First request to a new object:** 300 requests, each to an object never
  seen before (instantiate, run, commit), process CPU per request; then one
  more request to each (warm).
- **Private memory per live object:** `Private_Clean + Private_Dirty` from
  `/proc/self/smaps_rollup`, before and after the first requests, per object.

The guests (`experiments/preinit/guest`, built three ways by `build.sh`):
`GET /` reads `/data/count` and looks a key up in a table built on first use
(`LazyLock<HashMap>`): empty in `plain`, 50,000 entries in `heavy`;
`wizer-initialize` forces the table. The SQLite example is
`components/sqlite`'s app composed with the KV provider and with the SQLite
provider patched to export `wizer-initialize` (`sqlite-init.patch`: it calls
`sqlite3_initialize`, which sets up SQLite's allocator and page cache and
registers the VFS, and opens no file); `sqlite` is the patched provider as
built, `sqlite-pre` the same pre-initialized, so only pre-initialization
differs. Debug build of the node (its dependencies at opt-level 3), 4 cores,
load average 11-13 from other agents throughout; CPU, not wall time, is
compared.

| median (min-max), 5 rounds | bytes | compile, ms CPU | cached load, ms CPU | 1st request, µs CPU | warm request, µs CPU | private KiB/object |
|---|---:|---:|---:|---:|---:|---:|
| plain | 164,559 | 366 (349-413) | 3.4 (3.3-4.4) | 321 (289-373) | 89 (75-98) | 36 (36-75) |
| plain, pre-initialized | 164,332 | 365 (332-387) | 3.5 (3.1-3.7) | 312 (289-329) | 84 (70-93) | 32 (32-41) |
| heavy | 166,980 | 371 (371-390) | 3.7 (3.5-4.4) | 12,916 (12,058-13,698) | 81 (75-88) | 3,200 (3,200-3,202) |
| heavy, pre-initialized | 3,053,487 | 407 (366-448) | 5.9 (5.3-6.6) | 355 (273-435) | 91 (77-229) | 38 (37-42) |
| SQLite example | 1,694,893 | 4,403 (4,233-4,487) | 5.2 (4.7-6.6) | 4,342 (3,648-4,593) | 676 (630-786) | 283 (280-383) |
| SQLite, provider pre-initialized | 1,693,681 | 4,549 (4,301-4,717) | 5.2 (5.0-5.9) | 3,636 (2,922-4,837) | 688 (609-1,029) | 278 (276-286) |

300 objects for the Rust guests, 200 for SQLite (`GET /`: opens the
database, which a new object creates, and reads it). An earlier run of the
same SQLite pair (same components, load ~12) gave 4,354 vs 4,179 µs. Paired
by round over both runs, pre-initialized minus not: −1,421, −957, −619, −422,
−241, −114, +79, +253, +374, +419 µs: a median of about −180 µs with both
signs, so no measurable difference on this host. The first run of the Rust
guests (before the `/hash` route was added) agreed: heavy 14,891 → 354 µs.

For comparison, `notes/js.md` (branch `cf/optimistic-feynman-62f1a0`) measured
pre-initializing the JavaScript runtime with the same method: cold-start CPU
9.4 → 2.5 ms (3.7×), because evaluating an engine and an app is real work.

## Is it correct under nanovm?

- **Nothing from the host gets in.** The pre-initializer
  (`experiments/preinit/tool`, derived from `cf/onramp`'s `js-wizer`) defines
  every import as a trap. The `touch-data` build's initializer reads
  `/data/count`; pre-initializing it fails with `unknown import:
  wasi:filesystem/preopens@0.3.0#get-directories`, so no file, environment
  variable, clock or handle can be baked in; `build.sh` checks that it fails.
- **Std hash keys are shared by every instance.** Rust's std seeds
  `RandomState` once per thread from `wasi:random/insecure-seed` and keeps the
  keys in linear memory. Any `HashMap` built during initialization (even an
  empty one: `plain` too) asks for the seed, so pre-initialization fails
  unless the seed is answered (`--seed`, a constant, as `js-wizer` does), and
  then the keys are in the snapshot: every instance of every object, and
  every `HashMap` it creates later, uses keys anyone with the component can
  compute. Checked on a real node (`tools/local-stack`): `GET /hash` hashes
  `"x"` with a fresh `RandomState`; two objects of `plain` answer
  11605655794793628083 and 7250213639991139359, two objects of either
  pre-initialized build both answer 13901041566626661173. An app that keys a
  `HashMap` with client input becomes open to hash flooding; such apps should
  use `BTreeMap` or a keyed hasher seeded at run time. (W-640)
- **Handles and tasks.** Wizer snapshots linear memory and globals, not the
  component's handle tables. Imports can't create handles (they trap), but
  canonical built-ins can (`stream.new`, `waitable-set.new`, `resource.new`):
  an initializer that creates one leaves memory referring to a handle that
  doesn't exist at run time, and nothing detects it. Keep initializers to
  plain computation.
- **Behaviour.** Each pre-initialized component answered as the original
  through `tools/debug-run` and the local stack: `plain`/`heavy` `GET /` with
  the same table, and SQLite's `POST /set` and `GET /` with the same SQL,
  file and key-value results. Not run: the SQLite pressure matrix against the
  pre-initialized provider.
- **Composition.** Pre-initialize a component before composing it (Wizer
  handles one component's core modules, as `js/build.sh` does); the composed
  app keeps `wizer-initialize` only as an export of the part that had it,
  which the node never calls.

## Reproduce

```sh
export WASI_SDK_PATH=/path/to/wasi-sdk-34.0-x86_64-linux PATH=/path/to/wac-0.12:$PATH
bash components/sqlite/build.sh                 # the example app and KV provider
bash experiments/preinit/build.sh               # into target/preinit (the tool: ~8 min once)
T=target/preinit
cargo run --locked -p nanovm --example coldstart -- --rounds 5 --objects 300 \
    plain=$T/plain.wasm plain-pre=$T/plain-pre.wasm heavy=$T/heavy.wasm heavy-pre=$T/heavy-pre.wasm
cargo run --locked -p nanovm --example coldstart -- --rounds 5 --objects 200 \
    sqlite=$T/sqlite-orig.wasm sqlite-pre=$T/sqlite-pre.wasm
```

The pre-initializer is its own Cargo workspace on purpose: `wasmtime-wizer`
needs `wasm-encoder` features the node's build doesn't have, and adding it to
the node's dependencies rebuilds wasmtime. It builds wasmtime once (about 8
minutes on 4 busy cores, 1 GB in its target directory).
