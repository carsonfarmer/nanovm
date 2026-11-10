# Components over the filesystem (IDEAS section J)

Reusable building blocks shipped as Wasm components that an app composes with
`wac plug`, the way KV and SQLite already are. None adds a runtime line: each
is a provider over the object's own `wasi:filesystem` volume (or pure
computation), a small WIT interface, a minimal test app, tests on a real
in-process node, and a README. Branch `ideas/components`.

| Component | Interface | What it is | Provider size |
|---|---|---|---:|
| [queue](../components/queue/README.md) (J-90) | `nanovm:queue/log@0.1.0` | append-only topics; consumer positions; `next` reads and advances atomically with the request; trim | 199,971 B |
| [docs](../components/docs/README.md) (J-89, J-91, J-92) | `nanovm:docs/store@0.1.0` | JSON collections; unique and multi-value secondary indexes; range queries with cursors; full-text search (BM25); exact vector nearest neighbours; compaction | 296,671 B |
| [timeseries](../components/timeseries/README.md) (J-93) | `nanovm:timeseries/series@0.1.0` | raw points per day; hour/day rollups kept on write; aggregate at any step; retention per level | 218,793 B |
| [templates](../components/templates/README.md) (J-96) | `nanovm:templates/render@0.1.0` | minijinja: inheritance, includes, filters, escaping by name; pure computation | 886,064 B |
| [auth](../components/auth/README.md) (J-97) | `nanovm:auth@0.1.0` | JWT verify (inline JWK Set or URL fetched via egress and cached in the volume), argon2id, sessions | 627,414 B |
| KV, for reference | `spin:key-value@3.0.0` | | 176,080 B |

No standard WIT fit any of them (`wasi:messaging` is a broker client with
pushed delivery, not a stored log), so each has its own small `nanovm:`
package, following the KV/SQLite conventions: a resource opened by name,
async calls for anything that touches the filesystem, an `error` variant.

**Not built as components:** rate limiting (J-95) is better as an SDK helper
(ISSUES W-621): the logic is a dozen lines, and the real question is whether
each allowed request should become a committing write (about 1.2-1.5 ms CPU
plus a commit's fsyncs, see the KV and queue rows below) or keep state in memory and
fail open on eviction. `examples/ratelimit` on `ideas/examples` already shows
the durable version. Templates are built, but measured no faster than
minijinja linked into a Rust app, so for Rust the library is the simpler
choice; the component serves other languages.

## Using one

```sh
export PATH=/home/user/wt/tools/bin:$PATH        # wac 0.12
bash components/queue/build.sh                   # or scripts/build-components.sh for all
wac plug app.wasm --plug target/components/queue/provider.wasm -o app-with-queue.wasm
```

In the app, generate bindings from the component's `wit/` (world `client`)
with wit-bindgen 0.62, like `components/queue/tests/src/lib.rs`. Several
providers compose into one app with repeated `--plug`. The host's import
allowlist rejects an app that imports an interface no provider supplies.

## Design rules that fell out (for the next component)

- **No internal write-ahead log.** The host commits all of a request's
  `/data` changes or none. What a component must guarantee instead is that
  each *call* is all-or-nothing within the request, because an app may catch an
  error and still answer 200, which commits. So: validate before the first
  change; on a refused change, put back what the call already changed (queue
  and docs truncate an append; timeseries keeps an in-memory undo list across
  its several files); if putting back fails, trap, which rolls back the
  request.
- **Appends are the cheap write.** The undo journal records only a file's old
  size on its first append in a request; an overwrite records the old bytes;
  a removal moves the file into the journal without copying. All three
  stateful components are log-shaped for that reason.
- **Reclaim space by unlinking, not truncating** (ISSUES N-650). Truncating
  committed bytes costs journal room equal to them, so an in-place compaction
  of a full volume fails. `docs` opens its old log, unlinks it, and rewrites
  live records from the open descriptor: no free space or journal room needed.
- **Instance-lifetime caches are safe.** A failed request (and a failed
  post-return commit) ends the instance, so in-memory state built from disk
  (queue segment lists, docs keydir and indexes, timeseries file sets, the
  JWK Set) never outlives a rollback, as long as a call updates it only after
  its change succeeded.
- **Defer cancellation.** Mutations run inside `wit_bindgen::block_on`, as KV
  does, so a caller dropping the future can't stop a change halfway.
  Shared state sits behind a `futures` async mutex taken *before* `block_on`.
- **Rollback happens at the next instance start.** Right after a failed
  request, the volume on disk still shows its changes; tests must make another
  request to the object before inspecting files.
- **Commits dominate small writes.** One write per request costs about the
  same whatever the component (1.1-1.9 ms CPU, 15-34 ms wall here, mostly the
  commit's fsyncs); inside one request, a queue append costs ~67 µs, a doc put
  ~52 µs and a time-series point ~2 µs. Batch where the app can.

## Cost

`cargo run --locked -p nanovm --example components_bench -- [filter] --n 500`:
each case sends serial requests to one object of the component's test app on
an in-process node (no sockets), and reports wall time and process CPU
(user+sys from `getrusage`, so host work, wasmtime and the commit thread
included) per request, CPU per operation, and logical bytes added to the
volume per operation. Dev profile with optimized dependencies (the repo's test
profile; nanovm itself at opt-level 1). This VM had 4 cores at a load average
over 30 from other agents' builds: wall times are noisy, CPU less so. Three
rounds; median and [range] of CPU per operation.

| component | case | CPU µs/op, median [range] | wall µs/request, median | bytes/op on disk |
|---|---|---:|---:|---:|
| keyvalue | reference: KV set 9 B / request | 1,457 [1,454–1,774] | 25,409 | 0.0 |
| keyvalue | reference: KV get / request | 139 [121–167] | 154 | - |
| queue | baseline: unknown route (404) | 88.2 [69.8–105] | 112 | - |
| queue | append 1 x 100 B / request | 1,323 [1,207–1,394] | 18,178 | 116.0 |
| queue | read 1 / request | 173 [132–233] | 172 | - |
| queue | next 1 (consume + commit position) / request | 1,220 [1,209–1,266] | 19,306 | 0.0 |
| queue | append batch of 100 x 100 B / request (per message) | 15.9 [14.2–17.8] | 21,572 | 116.0 |
| queue | append 100 B x 1000 in one request (per append) | 67.1 [60.4–89.8] | 122,995 | 116.0 |
| docs | baseline: unknown route (404) | 71.0 [69.0–114] | 120 | - |
| docs | put 1 doc (~150 B, 1 index) / request | 1,078 [1,015–1,154] | 15,516 | 163.0 |
| docs | get 1 doc / request (of 10,000) | 89.1 [87.1–125] | 124 | - |
| docs | query tag = t3, 10 docs / request (of 10,000) | 130 [127–278] | 157 | - |
| docs | put 1,000 docs in one request (per put) | 51.9 [49.9–60.2] | 90,460 | 149.9 |
| docs | cold start: first get after restart, 10,000 docs, 1 index | 34,092 [33,010–39,396] | 60,880 | - |
| docs | cold start: first get after restart, 10,000 docs, no index | 13,983 [12,763–14,434] | 19,187 | - |
| docs | cold start: unknown route after restart (no load) | 523 [518–537] | 535 | - |
| docs | search 1 term in 1,000 of 10,000 docs, top 10 / request | 729 [682–793] | 736 | - |
| docs | nearest 10 of 10,000 64-dim vectors / request | 2,089 [2,086–2,222] | 2,281 | - |
| timeseries | baseline: unknown route (404) | 72.1 [70.8–93.9] | 73.0 | - |
| timeseries | write 1 point / request | 1,871 [1,867–2,136] | 34,014 | 16.1 |
| timeseries | write 1,000 points in one call (per point) | 2.2 [2.1–2.3] | 32,173 | 16.0 |
| timeseries | aggregate 3 days by day (rollups) / request | 124 [119–151] | 119 | - |
| timeseries | aggregate 1 day by hour (rollups) / request | 133 [116–146] | 131 | - |
| timeseries | aggregate 1 day by 15 min (1,440 raw points) / request | 310 [308–397] | 309 | - |
| timeseries | range 1 hour (60 raw points) / request | 210 [175–283] | 252 | - |
| templates | baseline: unknown route (404) | 56.3 [53.4–104] | 56.0 | - |
| templates | render a 20-row page via the component / request | 255 [248–259] | 259 | - |
| templates | render a 20-row page via the linked crate / request | 235 [234–289] | 237 | - |
| templates | 100 renders via the component (per render) | 139 [133–145] | 16,695 | - |
| templates | 100 renders via the linked crate (per render) | 125 [113–126] | 12,849 | - |
| auth | baseline: unknown route (404) | 72.7 [44.2–89.7] | 97.0 | - |
| auth | verify RS256 (inline JWK Set) / request | 1,376 [1,339–1,408] | 1,466 | - |
| auth | verify ES256 (inline JWK Set) / request | 2,287 [2,218–2,342] | 2,521 | - |
| auth | verify EdDSA (inline JWK Set) / request | 381 [378–384] | 383 | - |
| auth | argon2id hash, 19 MiB x 2 passes (default) / request | 62,432 [60,845–62,709] | 110,693 | - |
| auth | argon2id hash, 8 MiB x 1 pass / request | 11,391 [10,901–11,512] | 13,156 | - |
| auth | session create / request | 1,229 [1,155–1,315] | 16,129 | 71.0 |
| auth | session get / request | 159 [146–169] | 159 | - |

Reading it: subtract the component's "baseline" row (a request that opens
the component and answers 404) to get the cost of the operation itself.
Rollup aggregates read a few slots, so they cost about a raw `range` of an
hour. ES256 and RS256 are slow in Wasm because the RustCrypto big-integer and
field code runs on 32-bit limbs on wasm32; switching `rsa` to 64-bit digits
made RS256 slower (2.2 vs 1.3 ms), since Wasm has no 128-bit multiply.

Cold start of `docs` (ISSUES W-622): the first call after an instance starts
loads the whole log: 34 ms CPU for 10,000 documents with one index (14 ms
without an index; 0.5 ms for a request that doesn't touch the collection;
26 and 10 ms in a quieter earlier run).
Size grows linearly: keep collections under ~50,000 documents at the default
`cpu_ms` of 200.

Disk per item (logical bytes): queue message = payload + 16; doc = id + body
+ 9 (plus garbage until compaction, at most the live size); time-series point
= 16 (+ 32 per hour and per day with data); session = ~70 bytes of JSON plus
the app's data, one file each.

## Tests

`crates/nanovm/tests/components.rs`, 21 tests, `#[ignore]`d by default because
they need the built fixtures; CI builds them (`scripts/build-components.sh`)
and runs `cargo test --locked -p nanovm --test components -- --ignored`.

| Component | Tests |
|---|---|
| queue | `queue_contracts`, `queue_consumers_persist_across_eviction_and_restart`, `queue_failed_request_rolls_back_appends_and_positions`, `queue_segments_roll_and_trim`, `queue_quota_exhaustion_is_refused_cleanly` |
| docs | `docs_contracts`, `docs_indexes_survive_eviction_and_restart`, `docs_failed_request_rolls_back`, `docs_log_is_compacted`, `docs_quota_exhaustion_is_refused_cleanly`, `docs_full_text_search`, `docs_vector_nearest_neighbours` |
| timeseries | `timeseries_contracts`, `timeseries_rollups_survive_eviction_restart_and_raw_trim`, `timeseries_failed_request_rolls_back`, `timeseries_quota_exhaustion_is_refused_cleanly` |
| templates | `templates_contracts` (stateless: nothing to persist, roll back or fill) |
| auth | `auth_jwt_and_password_contracts`, `auth_jwks_url_is_fetched_once_and_cached_in_the_volume`, `auth_sessions_persist_expire_and_roll_back`, `auth_sessions_quota_exhaustion_is_refused_cleanly` |

Each stateful component's undo was checked by breaking it: removing the
queue's append undo, the docs append undo, the time-series undo list or the
session-create cleanup each fails that component's quota test, and each passes
again restored.

## Limits worth knowing

- Cooperative ownership: an app can still write `/data/.queue`, `.docs`,
  `.ts` or `.auth` directly and confuse the component, as with SQLite's file.
- File counts: queue 2 files per segment (~1 MiB) + 1 per consumer; docs 2 per
  collection; timeseries 1 per day of raw points; sessions 1 each. The default
  `max_files` is 1,000.
- Everything is per object. Another object reaches a queue, collection or
  session store by calling its object.
- No component can wait for another request (one request at a time per
  object): a queue consumer polls, it doesn't block.
