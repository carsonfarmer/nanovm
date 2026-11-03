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
- Keys are 1–180 UTF-8 bytes, encoded as unpadded base64url filenames. Values are
  at most 1 MiB. Empty values work. Missing reads return `none`; missing deletes
  succeed. No atomics or batch extension is supplied.
- Set writes an exclusive temporary file, closes it, and renames it over the key.
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

`cargo run --locked -p nanovm --example kv_stress` exercises 100/1,000/10,000 keys,
replacement, byte/file limits, exact on-disk values and rollback/restart. It samples
journal usage every 10 ms; those timings are diagnostic, not uninstrumented
benchmarks. On this orb, 10,000 three-byte values occupied 30,000 logical bytes
but 5,120,000 allocated bytes, excluding directory metadata. Logical quotas are
not physical disk reservations. A separate test replaces one value 1,000 times
inside one request: 221,721–226,722 bytes of undo metadata and only the four-byte
original retained; commit and rollback both pass. No journal data-format change.

## Performance and source accounting

Three alternating release runs compare the saved native host/counter from
`ef3a6b2` with this migration, with no overlapping builds/services. `kv_bench`
uses the in-process Node's HTTP request API (not a network server), checks exact
responses, and measures 1,000 reads and 200 writes at each size plus 100 cold
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
operations than the old in-place KV write. These explain candidate costs, not a
completed attribution profile. The no-regression adoption gate is **not met**.

Host count: 5,336 → 5,074 using the historical metric. That baseline accidentally
counts 36 lines of test-only native-KV setup; excluding those gives 5,300 → 5,074.
The replacement provider adds 213 production lines (test hooks excluded), making
the real runtime total **5,300 → 5,287: only 13 lines saved**. Build/setup scripts
grow by about 25 code lines, plus CI wiring. This is a smaller trusted host and
one mutation path, not an overall LOC win. Native atomics/batch were also removed,
so do not call the comparison feature-equivalent. The provider is 177,313 bytes;
counter grows from 193,471 to 392,960 bytes after composition.

Successor prompt: continue `spin-kv-prototype`, read HANDOFF Q75 and W-18. Preserve
filesystem-only persistence, Spin WIT and injected env/config. Do not merge on
the strength of correctness alone: profile the remaining ABI/I/O/journal costs
and decide whether the smaller trusted host justifies them. Do not batch undo
records across mutations without proving recovery at every crash boundary.
Branch pushes do not publish; merging guests into main needs owner approval.
