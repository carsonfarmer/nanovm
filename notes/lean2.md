# lean2: a smaller runtime after the merge (wave 2)

Branch `cf/lean2`, from the merged wave-1 branch at `2ee94bc`. The goal was
to bring the runtime (`crates/nanovm/src`, counted as CI does) back below
its pre-wave-1 size, 5,346 lines, by real simplification, keeping every
feature, fix, limit, error code and telemetry attribute, and to fix two
leftovers (N-110, N-91). The machine was shared with other agents' builds
(load average 7 to 12 most of the time), so timings are A/B pairs.

## Results, before → after

| | before (`2ee94bc`) | after (`cf/lean2`) | method |
|---|---|---|---|
| Runtime lines (CI's count) | 5,652 | 5,466 (−186) | the python snippet in `.github/workflows/ci.yml` |
| … of which real simplification | | −99 (net, N-110 adds 3) | the commits below, except the two test-code moves |
| … of which test code no longer counted | | −87 | test-only items that sat above CI's `#[cfg(test)] mod` cut |
| JS counter, first GET on a new object, p50 | 32.0 / 40.0 / 32.0 ms | 1.37 / 1.42 / 1.33 ms | js-bench (dev build), 200 cold starts, 3 alternating pairs |
| … process CPU per cold start | 4.76 / 4.40 / 4.87 ms | 1.75 / 1.57 / 1.38 ms | same runs |
| Distinct crates in a normal build | 265 | 265 | `cargo tree -p nanovm -e normal --prefix none`, `(*)` removed, `sort -u` |
| Release binary, unstripped / stripped | 48,633,944 / 33,581,520 B | 48,765,968 / 33,667,536 B (+0.26%) | `cargo build --locked --release -p nanovm --bin nanovm`, `strip` |

The binary grew by 86 KB stripped (0.26%). A symbol diff shows codegen
moving rather than a new dependency: the egress hook's futures were
restructured (the closures shift by about ±20 KB each, net +2 KB), the
proxy's two accept loops are one future, and two serde deserializers
(string maps, `Limits`) came out about 10 KB larger, plausibly from
`AppConfig` losing its defaults. No crate was added or removed.

**The target was not reached.** Every change below is a simplification or a
fix that stands on its own, but the runtime is still 120 lines above 5,346.
What was not done, and would close part of the gap: deleting the macOS
branches (36 lines that cannot compile today, N-131; kept, since the
direction is to restore macOS, not drop it), test support that lives in
the runtime (W-61, 19 lines), and larger changes in the files the hop
branch was rewriting, where only small separate hunks were allowed (W-60).
Beyond those I found no simplification that wouldn't remove a feature or
merely reformat code.

Runtime lines per file:

| file | before | after | | file | before | after |
|---|---|---|---|---|---|---|
| app.rs | 87 | 87 | | metrics.rs | 78 | 78 |
| config.rs | 147 | 141 | | object.rs | 698 | 659 |
| egress.rs | 429 | 407 | | proxy.rs | 246 | 245 |
| engine.rs | 109 | 109 | | quota.rs | 578 | 557 |
| jail.rs | 157 | 157 | | registry.rs | 383 | 374 |
| journal.rs | 472 | 464 | | server.rs | 114 | 107 |
| kv.rs | 198 | 201 | | supervisor.rs | 535 | 517 |
| lib.rs | 57 | 18 | | table.rs | 238 | 236 |
| link.rs | 186 | 186 | | telemetry.rs | 334 | 321 |
| main.rs | 175 | 174 | | ws.rs | 431 | 428 |

## The changes

Each is one commit (pick list at the end, with dependencies).

1. **kv: a bucket's directories are made by its first write (N-110).**
   Opening a bucket used to create `.kv` and `.kv/<bucket>`, journaled, so a
   request that only read a key paid a journaled create and a commit (two
   fsynced directory entries), and every new object spent two of its
   `max_files` on empty directories. Opening now only checks the name; the
   first `set` makes the directories (same entry check, journal record and
   count as before). A bucket with no directory reads as empty. The JS
   counter's first GET on a new object went from 32–40 ms to about 1.4 ms
   (table above). Test: `reading_a_new_bucket_makes_nothing` (fails if
   opening makes the directories, and if listing a missing bucket errors).
   Two tests leaned on the old behaviour and were adjusted: the hostile
   guest's kv `plant` makes `.kv/default` itself, and the JS eviction test
   lets `a` go idle before reading `b` (that read is now fast enough to
   finish first).
2. **journal: a touched path under what became a file no longer fails the
   commit (N-130).** Found through 1: a guest that makes a directory inside
   a missing one and then a file where the missing one would be made its
   own commit fail with `ENOTDIR` (only its own request rolled back). Such a
   path is gone just as a removed one is. Test:
   `a_path_under_what_became_a_file_commits` (fails without the fix).
3. **tests: `a_node_runs_at_most_max_apps` waits for the slow requests
   (N-91).** It slept 200 ms and assumed the request had reached its worker.
   It now waits for the worker to report the request in its guest. On two
   cores shared with four busy loops the old test failed 3 of 3, the new one
   passed 3 of 3.
4. **egress: one helper each for request span attributes and permit-held
   bodies (W-41)**, and later the same helpers in `supervisor.rs` and
   `table.rs` (small separate hunks). `telemetry::request` sets method and
   path on every request span; `egress::holding` ties a permit (or the
   worker) to a response body; `to_guest`, `empty` and `verdict` replace
   repeated response plumbing. Same attributes, verdicts and bodies.
5. **quota: one entry-limit check, and rename removes through the journal
   directly.** `reserve` repeated `check_new_entry`; `QuotaGuard::remove`
   existed only for `rename`.
6. **config: `AppConfig` has no defaults to fill in.** It is only ever read
   by a worker from the spec its supervisor wrote whole, so its six
   `#[serde(default)]`s and `deny_unknown_fields` guarded nothing.
7. **telemetry: varints through prost.** The wire-level stamping (N-74)
   carried its own varint reader and writer; prost (already used for OTLP)
   has both as public API. A 10-byte varint that overflows 64 bits is now
   refused as malformed, as prost's decoder does.
8. **journal: one wrapper for the two steps that poison the journal on
   failure** (`restore`, `cancel_rename`). It never clears the flag, so it
   can't un-poison a journal an earlier step left needing recovery.
9. **quota: a write stream needs no `Drop` to report success**: the
   awaited future already reads a dropped sender as success.
10. **registry: cache I/O through `tokio::fs`**, which is the
    `spawn_blocking` the code wrapped by hand (N-19).
11. **telemetry: one `cancelled` guard** for the front ends' and the egress
    spans (same `error.type = cancelled`; the status text is now
    "cancelled before response headers" on all of them).
12. **ws: the publish check is `messages` itself** (`well_formed` was a
    one-line wrapper).
13. **proxy: one serve loop for both listeners** (public and publish).
14. **quota and egress:** the two `Access`-taking stock calls go through the
    existing `delegate!` macro; a guest's own timeout header is no longer
    removed before the supervisor's is set, since it is worth no more than
    the option it could pass and `send` caps both.
15. **supervisor and object:** `Worker` and `MemLimiter` take their zeroed
    fields from `Default`.
16. **journal: records read with `iter::from_fn`** instead of a
    hand-written collect loop.
17. **Test-only code moved below CI's cut** (no code change, the count
    corrected): the counting allocator in lib.rs (declared `pub(crate)
    mod`, which the CI pattern misses), quota's `usage()`,
    `ObjState::for_test`, and `server::ok_json` (only tests called it; the
    integration tests have their own copy now). 87 lines.
18. **tests: `only_empty_scratch_is_reused` waits for the directory, not the
    store (N-132).** It failed once in a full run: a store is uncounted
    before its scratch directory is released. The runtime ordering is
    recorded as N-132 (object.rs was the hop branch's).

## Not done, and why

- **W-42 (one TLS trust store):** tried and measured: it drops no crate.
  `aws-lc-rs` stays for `sigstore-crypto`, and every reqwest rustls feature
  brings `rustls-platform-verifier`. Only the trust decision would change,
  so it was reverted (ISSUES W-42 updated).
- **W-40 (std `Mutex`, tokio's features named):** measured 265 → 262 crates
  with `cargo tree` (the feature list is in ISSUES W-40). Not done: half the
  lock sites are in the hop branch's files, a non-poisoning wrapper costs
  about 10 runtime lines, and tokio's own locks change, so it needs the HTTP
  A/B of CPU per request, and a rebuild of tokio and everything above it.
- **macOS (N-131, open):** link.rs uses rustix flags that exist only on
  Linux-like targets, so nanovm doesn't build on macOS. The fix (set
  `FD_CLOEXEC` after `recvmsg`, `SO_NOSIGPIPE`) needs a Mac to verify.
- **W-61:** `Supervisor::flush_telemetry` and the worker's `/_nanovm/flush`
  (19 lines) exist only for tests.
- **W-60:** smaller items in the hop branch's files (about −7 lines), and
  N-132's ordering.

## Tests

- Full suite (`cargo test --locked -p nanovm`, `RUST_TEST_THREADS=1`) after
  each group of changes, at load average 9 to 12: 117 unit, 74 integration
  (10 ignored, as on the base), 9 JavaScript, all passing on the final
  tree. Two failures seen on the way, each explained and fixed in the
  tests: `only_empty_scratch_is_reused` (N-132) and
  `counter_persists_across_eviction` (the eviction order change from 1).
- Production check without the `dev` feature: passes (`cargo check --locked -p nanovm --bin nanovm`).
- Mutation checks: N-110's and N-130's new tests fail without their fixes;
  N-91's old and new test were run on a loaded core (above).

## Pick list

In order. Each applies alone on `2ee94bc` except `9a6b99b`; `9a6b99b` and
`238bf82` need `760b3e0` (its helpers). The suite was run on the stacked
branch; building each commit alone was checked for the first three only
(the run was cut short by a container restart). Δ is CI's runtime line
count.

| commit | change | depends on | Δ |
|---|---|---|---|
| `ad0ccaa` | kv: buckets made by their first write (N-110), with the README line, the hostile guest's `plant` and the JS eviction test | — | +3 |
| `62c5104` | journal: a path under what became a file doesn't fail the commit (N-130) | — (found through `ad0ccaa`) | 0 |
| `e27f59a` | tests: `a_node_runs_at_most_max_apps` waits for the slow requests (N-91) | — | 0 |
| `760b3e0` | egress: request-span and permit-body helpers (W-41) | — | −12 |
| `0e23bbe` | quota: one entry-limit check; rename removes through the journal | — | −5 |
| `cb0e049` | config: `AppConfig` without defaults | — | −6 |
| `e205632` | telemetry: varints through prost | — | −19 |
| `d8e3a9f` | journal: `or_poison` | — | −5 |
| `cefdfcd` | quota: no `Drop` for the write stream's result | — | −7 |
| `cf16c73` | registry: cache I/O through `tokio::fs` | tokio's `fs` feature (in `full`; keep it if W-40 lands) | −9 |
| `9a6b99b` | telemetry: one `cancelled` guard | `760b3e0` (same lines) | −3 |
| `9b39f93` | ws: `messages` is the publish check | — | −3 |
| `c356402` | proxy: one serve loop | — | −2 |
| `aa2d770` | test-only code below CI's cut (lib.rs allocator, quota `usage`) | — | −44 |
| `1122832` | test-only helpers out of the runtime (`for_test`, `ok_json`) | — (touches object.rs, server.rs: hop's) | −43 |
| `6cf4798` | tests: `only_empty_scratch_is_reused` waits for the directory (N-132) | — | 0 |
| `238bf82` | supervisor, table: use the W-41 helpers | `760b3e0`; touches hop's files | −12 |
| `c898bee` | quota, egress: `delegate!` for two stock calls; one header removal | — | −5 |
| `7f7f18d` | supervisor, object: `Default` for `Worker`, `MemLimiter` | — (hop's files) | −11 |
| `1872b6a` | journal: `iter::from_fn` in `decode` | — | −3 |

## Reproducing

- Line counts: the CI snippet; per commit, the same over `git show
  <commit>:<file>`.
- JS cold start: build `--example js-bench` at `2ee94bc` and at this branch
  (dev profile), then alternate `js-bench --rounds 1 --cold 200 --warm 500
  js=js/target/counter.wasm`, 3 runs each.
- Crates: `cargo tree --locked -p nanovm -e normal --prefix none | sed
  's/ (\*)//' | sort -u | wc -l`.

## For the orchestrator

- Docs: README's kv paragraph now says buckets are made by their first
  write (in the N-110 commit). PLAN.md's "portable core runs on macOS" is
  currently untrue (N-131).
- `cargo build -p nanovm` (non-test, dev) builds a second full set of
  dependencies next to the test build's; use `cargo test --no-run` or
  `cargo check` in these worktrees (it cost 1.3 GB here, since deleted).
- I wrote `scratchpad/loc.py` at the scratchpad root once at the start
  (02:27), before moving everything under `scratchpad/lean2/`; if another
  agent had a file of that name it was overwritten.
