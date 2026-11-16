# Filesystem-backed Spin KV

Branch migration under evaluation; not ready for production adoption. The provider exports
unchanged `spin:key-value/key-value@3.0.0` from Spin v4.2.6. The vendored WIT keeps
its upstream Apache license. Compose it into an app with `wac plug`; the host
links only the filesystem, not KV. Env and config remain host-injected.

```sh
bash components/keyvalue/build.sh --test-hooks
cargo run --locked -p nanovm --example spin_kv_probe -- --test-hooks
bash scripts/build-guests.sh
```

`provider.wasm` is always the normal build; fault hooks only enter
`tests-faults.wasm`. The output is under `target/keyvalue`. No build publishes
packages. `scripts/build-guests.sh` composes counter and hostile; SQLite build
scripts compose both providers. Old synchronous KV imports are no longer linked.

## Storage contract

- A store is `/data/.kv/<label>`; valid labels are 1–64 ASCII letters, digits,
  underscores or hyphens. All such labels are allowed within an object's own
  volume. This differs from Spin's deployment-manifest label allowlist; nanovm
  does not interpret `spin.toml` or offer cross-object/shared stores.
- Opening a store only validates its label and reserves a resource slot. Its
  first write creates the directories; reads, listings and deletes of an absent
  store create nothing, including when the object's file quota is zero.
- Keys are 1–180 UTF-8 bytes, encoded as unpadded base64url filenames. Values are
  at most 1 MiB. Empty values work. Missing reads return `none`; missing deletes
  succeed. No atomics or batch extension is supplied.
- Set writes an exclusive temporary sibling, closes it, and renames it over the key.
  Existing descriptors continue to see the old file. KV and direct file access
  see each other's successful changes immediately; neither maintains a cache.
- Replacement needs space for the old and new values and one temporary entry.
  The host separately bounds journal logs and retained files. Catching a failed
  set preserves the old key and removes the temporary file; failed cleanup traps
  to force request rollback. Existing temporary-name collisions are skipped,
  at most 33 candidates, without modifying a pre-existing file.
- Mutations defer caller cancellation until publication or cleanup completes.
  Whole-request traps, deadlines and crashes still use host rollback. The
  provider never commits: the host's two existing commit boundaries apply to
  SQL, KV and ordinary files together.
- Listings stream one key at a time with a separate error completion. At most
  32 combined stores/listings are active; a listing retains its slot until its
  completion is consumed or dropped. Drop both endpoints to abandon a listing.
  Endpoint-close processing is asynchronous. Host resource limits additionally
  bound outstanding endpoints, including rejected listings.

## Validation checkpoint

Executed regressions failed before their fixes for unread completion-slot leaks,
pre-existing temporary-name collisions, and cancellation after temporary-file
creation. All three pass after fixes, along with CRUD, raw-file/descriptor
visibility, streamed listing/backpressure, concurrent replacement, quota cleanup,
trap rollback and restart. The cancellation fixture observes a filesystem marker
inside a started write before dropping its future; it is not an unpolled-call test.

Full migrated suite passes (91 unit, 67 integration); optional SQL tests include
mixed recovery cuts and jailed SIGKILL. Four KV-specific cuts recover after temp
creation, partial write, completed write and rename. A 100-object SQL+KV test
exposed pool exhaustion at object 53; pool totals now match the existing per-app
core/memory/table allowances, without increasing guest memory limits. Opening
existing stores no longer attempts mkdir, so full stores remain readable.

A stress run (`kv_stress`, since removed; it is in git history) exercised 100/1,000/10,000
keys, replacement, byte/file limits, exact on-disk values and rollback/restart. It sampled
journal usage every 10 ms; those timings are diagnostic, not uninstrumented
benchmarks. On this orb, 10,000 three-byte values occupied 30,000 logical bytes
but 5,120,000 allocated bytes, excluding directory metadata. Logical quotas are
not physical disk reservations. A separate test replaces one value 1,000 times
inside one request: 221,721–226,722 bytes of undo metadata and only the four-byte
original retained; commit and rollback both pass. No journal data-format change.

## Performance and source accounting

Three alternating release runs compare the saved native host/counter from
`ef3a6b2` with this migration, with no overlapping builds/services. `kv_bench`
(since removed with the native host; it is in git history) used the in-process Node's
HTTP request API (not a network server), checked exact responses, and measured 1,000 reads and 200 writes at each size plus 100 cold
objects. Medians are milliseconds/request:

| Operation | Native | Composed |
|---|---:|---:|
| GET 128 B | 0.105 | 0.185 |
| GET 16 KiB | 0.134 | 0.241 |
| GET 1 MiB | 1.413 | 3.155 |
| PUT 128 B | 1.983 | 2.952 |
| PUT 16 KiB | 2.568 | 3.437 |
| PUT 1 MiB | 4.614 | 7.126 |
| Cold PUT | 3.142 | 4.149 |

Reading into a bounded growing buffer avoids per-chunk allocation/copy but does
not close the gap. Spin streams **keys**, not values: get/set still carry a
`list<u8>` across component boundaries. Atomic rename also performs more journal
operations than the old unlink-and-create KV write. These explain candidate costs, not a
completed attribution profile. The no-regression adoption gate is **not met**.

Host count: 5,336 → 5,074 using the historical metric. That baseline accidentally
counts 36 lines of test-only native-KV setup; excluding those gives 5,300 → 5,074.
The replacement provider adds 213 production lines (test hooks excluded), making
the real runtime total **5,300 → 5,287: only 13 lines saved**. Build/setup scripts
grow by about 25 code lines, plus CI wiring. This is a smaller trusted host and
one mutation path, not an overall LOC win. Native atomics/batch were also removed,
so do not call the comparison feature-equivalent. The provider is 177,313 bytes;
counter grows from 193,471 to 392,960 bytes after composition.

## Follow-up: measured costs, not inherent Wasm overhead

The native `set_value` first reserved quota, moved the old key into the journal,
then created/wrote its final path directly. Failures after reservation trapped
the request. It was **not** an in-place overwrite. The composed provider stages
the new value first so a catchable failure preserves the old key without aborting
other changes. Copying the native sequence into Wasm would require different
error semantics or a filesystem operation that reserves and replaces atomically.

Across `kv_bench`'s 703 sets, syscall counts were native/composed:
`fdatasync` 1,507/2,210, `fsync` 4,420/5,022, and total rename calls 603/1,306.
The retained optimization puts temporary files inside the bucket, avoiding a
second modified directory: `fsync` returns to 4,420. It does not remove the extra
undo record or weaken ordering. Small-write timing differences were noisy, so
claim the syscall reduction, not a proven wall-time improvement. Longer temporary
paths increase the 1,000-replacement undo log to about 238–243 KB (still 4 retained
seed bytes); runtime LOC is unchanged.

More importantly, a `perf record -e cpu-clock:u -F 997` sample attributed about
one third of user CPU to generated adapter module 6, function 32. Wasmtime 50's
`fact::translate_list` emits a per-element translation loop even for `list<u8>`.
The [diagnostic patch](../../experiments/wasmtime-byte-list-copy.patch) replaces
that loop with `memory.copy` for wasm32 byte lists, retaining allocation/range
checks. This is **not enabled in Cargo.toml or Cargo.lock**. Three alternating
native/stock/patched runs, identical composed guest for the last two, measured:

| Operation | Native | Composed stock | Composed bulk-copy experiment |
|---|---:|---:|---:|
| GET 128 B | 0.108 | 0.185 | 0.206 |
| GET 16 KiB | 0.127 | 0.232 | 0.230 |
| GET 1 MiB | 1.404 | 3.149 | 1.600 |
| PUT 128 B | 1.987 | 2.720 | 2.798 |
| PUT 16 KiB | 2.509 | 3.247 | 3.290 |
| PUT 1 MiB | 4.925 | 7.531 | 5.854 |
| Cold PUT | 3.164 | 4.219 | 4.011 |

An additional three pairs with sibling temporary files measured stock/patched
1 MiB GET 3.161/1.390 ms and PUT 7.333/5.690 ms. They are a separate series, not
a simultaneous native comparison. Small calls still carry filesystem capability
resolution, WIT resource/async-call overhead and metadata work absent from the
native shortcut. Both hosts already use inline filesystem calls; this was not a
new `spawn_blocking` cost. A stat-sized read-buffer experiment did not improve
large reads and was reverted.

Reproduce the diagnostic dependency in a disposable checkout: copy the fetched
`wasmtime-environ-50.0.0` crate into an isolated directory, apply the patch with
`patch -p3` there, and build `kv_bench` (from git history) with Cargo's
`--config 'patch.crates-io.wasmtime-environ.path="/absolute/copied/crate"'`.
Save that binary separately; restore Cargo.lock and build the stock control.
Use the same composed `counter.wasm` for both. `kv_bench` creates a fresh compiled
cache per run. Do not modify Cargo's registry cache. For profiling, temporarily
enable `Config::profiler(ProfilingStrategy::PerfMap)` and correlate sampled PCs
with `/tmp/perf-<pid>.map`; restore engine.rs afterwards.

The patched engine passes the full component probe, including varied binary
values, cancellation, quotas, interleaving and recovery cuts. This is not an
upstream compiler test suite or validation of all aliasing, memory64 and malformed
ABI cases. Prefer an upstream fix with those tests over silently shipping a fork.
The retained sibling-temp change passes the stock full locked suite (91 unit,
67 integration, eleven ignored), stock component probes and component clippy.
Disabling collision retries fails the relocated sentinel test; restored with
`git checkout --` and empty diff confirmed, then the complete probe passes again.

The storage contract is coherent **tentative** state that can be rejected and
committed state that meets the chosen durability policy, not an undo-log mandate.
Snapshots, copy-on-write and transactional virtual filesystems remain candidates.
For external durability/regions, separate tentative-state mechanics from ordered
committed versions and acknowledgement policy; see PLAN's replication discussion.

Successor prompt: continue `spin-kv-prototype`, read HANDOFF Q75 and W-18. Preserve
filesystem-only persistence, Spin WIT and injected env/config. Do not merge on
the strength of correctness alone: validate/upstream the byte-list adapter fix,
then profile small-call and remaining write costs. Do not batch undo
records across mutations without proving recovery at every crash boundary.
Branch pushes do not publish; merging guests into main needs owner approval.
