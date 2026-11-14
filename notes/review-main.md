# Review of main (`0f9ee0f`) outside the runtime: what to cut

Scope: everything on `origin/main` except `crates/nanovm/src/*.rs`, which the
`trim/storage`, `trim/node` and `trim/edge` agents own. Nothing was built.
Files were read in a plain worktree with no `target/`.

**How the numbers were counted.** Lines are physical lines (`wc -l`) at
`0f9ee0f`. Generated files (`Cargo.lock`, a benchmark JSON) are counted
separately from hand-written ones. Some doc lines run past 1,000 characters,
so line counts understate the doc cuts. The KB figures show their real size.

**Tools.** `cargo machete` is not installed. Its GitHub release download is
blocked by this sandbox's proxy, and building it was off limits. I ran the
same static check myself instead: every key under `[dependencies]` and
`[dev-dependencies]` in all 46 `Cargo.toml` files, searched for in that
crate's sources. `cargo tree -d --offline` ran without making a `target/`.

## Summary

In scope: 50,187 lines. Of these, 32,441 are hand-written and 17,746 are
generated (11 lockfiles and one JSON file).

| | Hand-written | Generated | Total |
|---|---:|---:|---:|
| **Safe now** (21 cuts) | 6,060 | 8,589 | **14,649** |
| **Owner decision** (14 cuts) | 9,947 | 1,763 | **11,710** |
| Owner decision, keeping JS (recommended) | 8,174 | 987 | 9,161 |
| **All** | 16,007 (49% of hand-written) | 10,352 | **26,359 (53%)** |

The biggest wins:

- Finished experiments (`experiments/fs-backends`, the SQLite mode benchmark):
  11,232 lines, safe now.
- History in the top-level docs: README, PLAN and HANDOFF lose 1,221 lines
  (214 KB of their 326 KB), safe now. ISSUES' 101 closed entries (650 lines,
  104 KB) need your call.
- Example apps, 24 → 9: 4,075 lines. Every SDK feature stays covered once.
- `.agents` vendored style guides and Amp orb setup: 1,739 lines.

Two cuts help the trim agents. `experiments/fs-backends` and
`examples/committed_state.rs` are the only code outside the runtime that
uses `nanovm::journal::Journal` and `nanovm::object::ObjShared`. Once they
are gone, `trim/storage` can make both private.

**`beta/integration` (`7988cae`)** sits under all three trim branches. Every
file this review cuts still exists there unchanged, except the restic backup
(O7), which beta already removed. Beta also reworks the SDK (O12). It adds
about 9,400 lines outside the runtime (`deploy/`, `docs/ops`,
`tools/bench-history`, `tools/release`, `notes/beta-*`). Those are not on
main and are not reviewed here.

## All cuts, ranked by size

| # | Cut | Lines | Flag |
|---|---|---:|---|
| S1 | `experiments/fs-backends/` (rejected page-store experiment) | 6,616 | safe |
| S2 | SQLite DELETE-vs-WAL benchmark: committed JSON, guest route, test, README section | 4,616 | safe |
| O1 | Example apps 24 → 9, with their tests and notes | 4,075 | owner |
| O2 | JavaScript runtime (`js/`, `js-wizer`, `tests/js.rs`) | 2,549 | owner (recommend keep) |
| O3 | `.agents/` vendored Rust style guides, grilling skill, Amp orb setup | 1,739 | owner |
| O4 | Per-fix evidence notes (6 files) and `multinode.md`'s test and review sections | 943 | owner |
| O5 | Lockfiles: one workspace for the three test-fixture crates (up to ~2,600 with all wasm crates) | 830 | owner |
| O6 | ISSUES.md: 101 closed entries | 650 | owner |
| S3 | HANDOFF.md: history marked superseded, plus copies of AGENTS.md | 502 | safe |
| S4 | `crates/nanovm/examples/committed_state.rs` (W-9 proof, superseded by `ship.rs`) | 454 | safe |
| O7 | Offline restic backup (`scripts/backup.py`, its tests, drill, CI, README) | 451 | owner (already gone on beta) |
| S5 | PLAN.md: simplification analysis, W-9 proof, phase-1 plans, layout | 410 | safe |
| S6 | `kv_bench.rs` (compares against the removed native KV) and `kv_stress.rs` | 409 | safe |
| S7 | `experiments/sqlite-filesystem/README.md` proposals and history | 345 | safe |
| S8 | README.md: telemetry history, storage history, old benchmarks, bug log, test list, layout | 309 | safe |
| O8 | Test guests onto the SDK (W-104); delete `guests/util` | ~250 | owner |
| S9 | Raw-bindings SQLite example and its runner (copy of `sqlite_composed_provider`) | 245 | safe |
| S10 | `notes/round3.md` (how to pick round-3 branches; it landed) | 183 | safe |
| S11 | Test helpers repeated in 4 test files → `support/` | ~150 | safe |
| S12 | `.agents/skills/writing-rust/references/bevy.md` (game engine) | 143 | safe |
| S13 | `components/keyvalue/README.md` migration narrative (native vs composed) | 118 | safe |
| O9 | Repeated docs: README quickstart vs SDK README; PLAN phases 2–7 vs README | 88 | owner |
| O10 | `.github/workflows/publish-app.yml` (a copy-me template) | 67 | owner |
| S14 | `scripts/openbench.c` (2026 macOS-vs-Linux `openat` check) | 42 | safe |
| O11 | HANDOFF.md remainder: Amp and old-sandbox gotchas; move Q table to PLAN | 33 | owner |
| S15 | `experiments/wasmtime-byte-list-copy.patch` (never applied) | 31 | safe |
| S16 | `stock.wasm` SQLite variant (repeats the refused-imports test) | 28 | safe |
| S17 | SQLite build flags written out 3 times → once | ~25 | safe |
| O12 | SDK public API no app or test uses | ~25 | owner |
| S18 | Rule 1: tests and demo that name the removed stats/events API | ~10 | safe |
| O13 | Guests: unused `/bigfile` route; duplicate `guests/rustfmt.toml` | 10 | owner |
| S19 | `egress_listed_host_allowed` (ignored, needs the internet, never run) | 8 | safe |
| S20 | Unused `serde` dependency in 4 examples | 4 | safe |
| S21 | ISSUES.md "Current delivery (2026-11-08)" header | 1 | safe |
| O14 | `base64` 0.22 → 0.23 (the rest of the tree uses 0.23) | 0 (−1 crate) | owner / trim agents |

## Experiments

**S1. `experiments/fs-backends/`: 6,616 lines (2,356 hand-written, 4,260 lock). Safe now.**
- *What:* the redb plain-page and hash-page backend comparison: `src/{main,inode,failure,capped}.rs`,
  a 592-line README and its own Cargo workspace and lock.
- *Evidence:* the decision is made and recorded: "retain journaling" (Q84,
  PLAN, README "Storage decision"). No CI step builds it; `git grep fs-backends`
  finds only doc links. It is a path dependency on `nanovm`
  (`use nanovm::{journal::Journal, object::ObjShared}`), so nothing checks it
  still compiles as the trim agents change those types.
- *Cost:* none in code or tests. The README links go too (counted in S5 and S8).
  Q84 in HANDOFF keeps the one-paragraph conclusion.

**S2. SQLite DELETE-vs-WAL benchmark: 4,616 lines (295 hand-written, 4,321 JSON). Safe now.**
- *What:* `experiments/sqlite-filesystem/benchmark-results.json` (raw output),
  `src/bench.rs` (121), its `/bench` route in `src/lib.rs` (4),
  `benchmark_database_modes` in `tests/integration.rs` (70, `#[ignore = "manual SQLite benchmark"]`),
  and the README section "DELETE versus exclusive WAL benchmark" (100).
- *Evidence:* deliberately outside the `sqlite_` filter CI runs. The JSON is a
  one-off result and the README records the conclusion. The test writes
  `target/sqlite-filesystem/benchmark-results.json`, so nothing reads the
  committed copy.
- *Cost:* the benchmark can't be rerun without git history.

**S7. `experiments/sqlite-filesystem/README.md`: 345 of its 556 lines. Safe now.**
- *What:* "Original amplification and the generic fixes" (68), "Component composition
  proof" (83), "Revised interface proposal" through "Permanent support" (105),
  "Scope for a production beta" (79) and "Query cancellation investigation" (10).
- *Evidence:* the proposals were delivered as `components/sqlite`, whose README holds the
  supported contract. Query cancellation is closed in ISSUES W-1. Keep the intro,
  "Reproduce", "What the tests exercise" and the WAL section (111 lines).
- *Cost:* design history, which lives in git.

**S15. `experiments/wasmtime-byte-list-copy.patch`: 31. Safe now.** A diagnostic
`fact::translate_list` patch, "not enabled in Cargo.toml or Cargo.lock" (KV README). The
finding (Wasmtime 50 copies `list<u8>` a byte at a time) belongs upstream or in one ISSUES
line, not as a dead patch.

**S16. `stock.wasm` SQLite variant: ~28. Safe now.** The `stock` feature in
`experiments/sqlite-filesystem/Cargo.toml`, its `build.sh` loop pass, the `#[cfg(feature = "stock")]`
branches in `src/lib.rs`, the first ten lines of `sqlite_over_the_existing_filesystem`, and the README
bullet. All it checks is that a component importing `wasi:filesystem@0.2` is
refused. `components_importing_refused_interfaces_are_rejected_by_name` already
checks that, using the `p2_filesystem` fixture (`std::fs`). Cost: none.

## Crate examples, test helpers and tests

**S4. `crates/nanovm/examples/committed_state.rs`: 454 (with its 4-line `[[example]] test = true`). Safe now.**
- *Evidence:* its header says "W-9 storage-only protocol experiment. Not a runtime
  replication API". It uses a JSON receiver format the runtime never used.
  Production capture, shipping and restore (`committed.rs`, `ship.rs`) replaced
  it, and `shipped_versions_restore_an_object_on_a_fresh_root`,
  `an_unreachable_store_withholds_acknowledgement_and_bounds_the_outbox` and the
  `nodes` tests cover them.
- *Cost:* its two built-in tests, which test only the proof's own receiver.
  `cargo test -p nanovm` builds every example, so test builds get a little
  faster.

**S6. `kv_bench.rs` (116) and `kv_stress.rs` (293): 409. Safe now.**
- *Evidence:* `kv_bench` is "Matched HTTP-level KV workloads for native and
  composed providers". The native provider is gone, so it measures a comparison
  that no longer exists, and it names an older version (rule 1). Neither file
  runs in CI. `git grep` finds them only in `components/keyvalue/README.md` and
  the fs-backends README.
- *Cost:* the manual 10,000-key scale run in `kv_stress`. `spin_kv_probe`
  (in CI) and the `kv_*` integration tests cover contracts, quotas, streaming,
  1,000 replacements, recovery cuts and restart.

**S9. Raw-bindings SQLite example and runner: 245 (237 hand-written, 8 lock). Safe now.**
- *What:* `components/sqlite/example/` (95), `crates/nanovm/examples/sqlite.rs` (93), the example
  member and its 3 lines in `components/sqlite/build.sh`, CI's "Run composed SQLite example" (2),
  and `components/sqlite/README.md` "Build and run" (49 → ~6 pointing at `nanovm_sdk::sqlite`).
- *Evidence:* `sqlite_composed_provider` (in CI's `sqlite_` step) checks everything
  the runner checks: persistence, isolation, mixed SQL/file/KV rollback and
  restart. It also checks failure paths. Users now get SQLite through
  `nanovm_sdk::sqlite` plus `tools/build-app.sh`, which composes the provider,
  as in `examples/shop` and `examples/notes`.
- *Cost:* the README's "SQLite beta" points at an SDK example instead of a raw-WIT one.

**S11. Test helpers repeated in 4 test files: ~150. Safe now** (test-only; run the suites
after).
- `repo()` appears ×4.
- `raw` appears ×2.
- WebSocket `open`/`next`/`say` appear ×3: `dev.rs`, `examples.rs` and
  `integration.rs` `mod sockets`.
- A `nanovm run` spawner appears ×2: `dev.rs` `Running` and `examples.rs` `App`.
- `node_cfg`/`node_with`/`node`/`settle` appear ×2: `integration.rs` and `js.rs`.

Move them into `crates/nanovm/support/` as `load.rs` already is. Move
`the_quickstart_app_counts_visits` and `the_chat_example_is_live_and_durable`
from `dev.rs` to `examples.rs`, where the other example tests are.

**S18. Rule 1, names of the removed stats/events API: ~10. Safe now.**
- `integration.rs`: the two `for path in ["/_nanovm/stats", "/_nanovm/events"] { … 404 }` loops
  (`http_front_end`, `supervised_workers_contain_attacks`). `/_nanovm/nope` → 404 already covers
  unknown private routes.
- Using `/_nanovm/stats` as a sample private path (lines 1212, 3689), the
  message "must not return a stats snapshot", and the helper name `stats_json`
  (it reads the OTLP snapshot).
- `demo/demo.sh:127`, which fetches `/_nanovm/stats`.
- In docs: README "Native synchronous KV, atomics and batch are removed", "no compatibility
  endpoints" and Layout's "replaces native KV on this branch". These are cut in S8.
  `components/keyvalue/tests/src/lib.rs:76` says "native KV files" when it means
  raw files.
- No code in scope reads, converts or migrates an older format.

**S19. `egress_listed_host_allowed`: 8. Safe now.** `#[ignore = "needs the internet"]`. CI
never runs it, and `egress_allowed_request_round_trips` covers the allowed path on loopback.

## Example apps, SDK and JavaScript

**O1. Example apps 24 → 9: 4,075 (3,918 hand-written, 157 lock). Owner decision.**
- *Keep:* hello (KV quickstart), chat (WebSockets, publish), shop (SQLite all-or-nothing,
  calls), notes (FTS5: the only test of N-603's pragma allowance), events
  (streamed body, SSE, post-return deadline), files (`/data`, ranges, quota),
  webhooks (`after`, egress), gateway (`Fetch::stream`, N-713's broken-upstream
  regression) and tictactoe (calls between objects plus sockets).
- *Cut:* shortener, ratelimit, locks, polls, leaderboard, jobs, site, presence, flags, wiki,
  analytics, reminders, todos, feeds, oauth.
- *Lines:* app code 2,778. Their tests in `examples.rs`: 910 (plus `add_after_restart`,
  `feed_server`/`rss`, `github`). `notes/examples.md` goes from ~273 to ~43. The
  lock drops 20 packages (157 lines: `pulldown-cmark`, `similar`, `roxmltree`,
  `unicase` and the members).
- *Why:* the cut apps repeat features the kept ones already exercise: KV per
  object (×9), SQLite per object (×6), sockets (×5), calls (×6). Each SDK
  feature stays covered once. The cookie API keeps its SDK unit tests.
- *Cost:* the showcase breadth. Three app-security review tests go with them:
  `review_wiki_*` (Markdown links), `review_oauth_*` and `review2_oauth_*`
  (cookie binding). Fewer components to build and test in CI.

**O2. JavaScript runtime: 2,549 (1,773 hand-written, 776 lock). Owner decision; recommend keep.**
- *What:* `js/` (1,255), `examples/js-wizer.rs` (87), `tests/js.rs` (419),
  `javascript_apps_run_and_publish` (12), the `wasmtime-wizer` dev-dependency
  and its CI build line.
- *Why keep:* you asked for JS guests (Q47, W-6), and it adds nothing to the
  node. Listed so the full cost is visible.
- *Stale doc:* `js/README.md` names `tests/run.rs` and `notes/js.md`/`qjs.md`;
  none of them exist on main.

**O12. SDK public API nothing uses: ~25. Owner decision.** `fs::tmp`, `Dir::descriptor`,
`fs::write_at`, `ws::publish_all` (fold into `publish`), `Answer::binary` and
`Answer::unsubscribe`. No example, test guest or fixture calls them. They are
small, but they are public API, and `beta/integration` rewrites the SDK on
`http` types, so decide there.

**S20. Unused `serde` dependency: 4. Safe now.** `examples/{flags,gateway,jobs,oauth}/Cargo.toml`
declare `serde` with `derive` but use only `nanovm_sdk::serde_json`. This was the
only real finding of the static dependency check. `sqlite-vfs` in
`experiments/sqlite-filesystem` showed up too, but it is used through the
`#[path]`-included `vfs.rs`, so that was a false alarm.

## Components and guests

**S13. `components/keyvalue/README.md` lines 49–181: 118 of 133. Safe now.** The
"Validation checkpoint", "Performance and source accounting" and "Follow-up"
sections compare against the removed native host (rule 1) and point at
`kv_bench`, `kv_stress` and the patch (S6, S15). They also end with a
"Successor prompt" for a finished branch. Keep the storage contract and a
5-line validation summary.

**S17. SQLite build flags written out three times: ~25. Safe now** (check it in
CI). The `CC_/AR_/CFLAGS_wasm32_wasip2` and `LIBSQLITE3_FLAGS` block is copied in
`components/sqlite/build.sh`, `experiments/sqlite-filesystem/build.sh` and
`tools/build-app.sh`. Keep it once, in `build-app.sh` ("the flags
components/sqlite/build.sh builds it with" already admits the copy). Also,
CI runs `components/keyvalue/build.sh` four times.

**O8. Test guests onto the SDK (W-104); delete `guests/util`: ~250 net. Owner decision.**
`guests/util` (287) mostly repeats what `nanovm-sdk` does. It is used by
counter, hostile, `tests/refused`, `tests/post-return`, the SQLite fixtures and
the S9 example. Changing `guests/**` republishes the signed guests through
`publish-guests.yml`, and `beta/integration` is reworking the SDK. Do this
after the SDK settles.

**O13. Guests, trivia: 10. Owner decision** (any change under `guests/**`
republishes). Hostile's `/bigfile` route is used by no test or demo (only PLAN
mentions it). `guests/rustfmt.toml` repeats the root `rustfmt.toml`, which
rustfmt finds anyway.

## Top-level docs

Four files say the same things over and over:

| Topic | Where it is repeated |
|---|---|
| What nanovm is | README intro, AGENTS.md, HANDOFF "What nanovm is", PLAN "Context" |
| Repository layout | AGENTS.md, README "Layout", PLAN "Layout", HANDOFF "Layout pointers". The last three are stale: PLAN lists `metrics.rs`; HANDOFF lists `kv.rs`, which is gone |
| How to work | AGENTS.md, HANDOFF "How to work with this owner" and "Working alongside other agents", PLAN "Working rules" |
| Telemetry (W-2) | README "Try it" (39 lines, 12 KB), PLAN roadmap item 2 and its details block, HANDOFF "W-2 completion" (199 lines, 65 KB), ISSUES W-2 (42 lines) |
| Storage backend decision | README KV section, PLAN simplification analysis, HANDOFF checkpoints, ISSUES W-18, `experiments/fs-backends/README.md` |
| W-9 | README top, PLAN "W-9 contract", HANDOFF "Current state", ISSUES W-9/W-80, `notes/storage.md`, `notes/multinode.md` |
| Runtime line count | README alone gives 5,320, 5,305, 5,315 and 5,085, all stale. CI reports the real one |

**S3. HANDOFF.md: 502 of 617 lines (135 KB of 163 KB). Safe now.**
- "Current state (2026-11-08)" (93): it predates round 3.
- "Historical checkpoints (superseded…)" (102), "W-2 completion and validation" (199) and
  "Completed W-3 brief (historical…)" (45): all marked historical.
- "What nanovm is" (9), "Phase 3 as built" (8, also in PLAN), "How to work with this owner" and
  "Working alongside other agents" (30, now AGENTS.md), "Layout pointers" (16, stale).
- What stays: the Q1–Q90 decision table (82 lines, the real record) and the
  environment gotchas (O11).

**S5. PLAN.md: 410 of 715 lines (36 KB of 72 KB). Safe now.**
- "Runtime simplification analysis (2026-11-03)" (208): a superseded analysis.
  Its outcomes are Q84–Q86, and the trim agents are now doing the cutting.
- "W-9 contract and bounded proof" (89 of 99): `ship.rs` delivered it. Keep the
  open acknowledgement/RPO decision (~10 lines).
- The roadmap's "Historical implementation and validation checkpoints" block (19).
- Phase-1 "Tests", "Benchmarks", "Demo" and "Order of work" plans (51), "Layout" (28),
  the stale "Context" status paragraph (8) and "Follow-ups" (7: W-12 is done; Wizer is done for JS).

**S8. README.md: 309 of 776 lines (43 KB of 91 KB). Safe now.**
- The "Current scope (2026-11-08)" and `committed_state` paragraphs (16).
- The W-2 measurement history in "Try it" (39): keep two lines on telemetry.
- The Collector prose (11): keep the pointer to `demo/otel-collector.yaml`.
- The storage-experiment narrative in "Key-value and config" (67).
- The phase-1 Mac and Linux benchmark tables and `openbench` notes (48): keep
  the command.
- "Tests" (48): stale counts ("64 integration and 93 unit") and a list of test
  names. Keep how to run.
- "Bugs the tests and benchmarks found" (53, a history log) and "Layout" (27,
  AGENTS.md has it).
- The design sections stay: the jail, the proxy, apps, calls, WebSockets,
  commits and host requirements.

**S10. `notes/round3.md`: 183. Safe now.** This is the guide to picking `cf/main-next` and
`cf/round3`, which have both landed. Before deleting it, carry its four open
items into ISSUES: N-228, N-670 and W-621/W-623 are not defined there. That
adds about 4 lines.

**S21. ISSUES.md "Current delivery (2026-11-08)" header: 1. Safe now.** It is stale (before round 3).

**O6. ISSUES.md: 101 closed entries, ~650 lines (104 KB of 147 KB). Owner decision.**
Every fixed or done entry already has its commit, test and message. Keep ISSUES
as the list of open work (33 entries). AGENTS.md then changes from "mark it fixed
with the commit" to "remove it in the fixing commit".

**O9. Docs that repeat each other: 88. Owner decision.** README "Build an app"
repeats `sdk/rust/README.md` "Five minutes" command for command (~25). PLAN
"Phase 2"–"Phase 7 (done)" (63) repeats README "The jail", "The proxy", "Apps",
"Commits" and "Calls". Pick one home for each.

**O11. HANDOFF.md remainder: 33. Owner decision.** Move the Q table into PLAN as
"Decisions" and delete HANDOFF. AGENTS.md "Build and test" already has what the
Amp-orb and old-sandbox gotchas (27) still matter for.

## Notes and `.agents`

**O4. Per-fix evidence notes: 943. Owner decision.**
- *Cut:* `cpuclock.md` (204), `hardening.md` (180), `sec.md` (149),
  `r3-worker.md` (88), `quality.md` (38), `ops.md` (27), plus `multinode.md`'s
  "Tests" catalogue (150, which repeats the test docstrings) and "Second lease
  review" (107).
- *Why:* AGENTS.md already requires each commit message to carry problem,
  evidence, test and line delta, and each test carries its docstring.
- *Keep:* the design notes: `multinode.md` (trimmed), `storage.md` and `onramp.md`.
- *Stale refs:* `ops.md`, `quality.md` and `examples.md` cite `IDEAS.md`,
  which is not on main.

**O3. `.agents/`: 1,739. Owner decision.**
- *Skills (1,676):* the vendored BeautifulRust `writing-rust` skill with 6
  reference digests, the `grilling` skill and their licenses. AGENTS.md now
  holds the rules that apply here.
- *Orb setup (63):* `setup` and `resume` serve Amp orbs only. `setup` still
  installs restic.

**S12. `bevy.md`: 143. Safe now.** A Bevy game-engine guide (plus one index
line in
`SKILL.md`). It has nothing to do with a Wasm host. Even if the skills stay, this goes.

## CI, scripts, workflows, dependencies

**O7. Offline restic backup: 451. Owner decision (already taken on `beta/integration`).**
- *What:* `scripts/backup.py` (112), `scripts/test_backup.py` (139),
  `sqlite_backup_restores_into_a_fresh_root` (107), README "Backup and restore"
  (81), CI's "Check offline backup tooling" (6, which runs the tests twice:
  unprivileged and as root) and the restic step in `.agents/setup` (6).
- *Why:* beta replaces it with "backup is the store" (ship and restore).
  Keeping it on main means one more apt package and two test runs per CI run.

**O5. Lockfiles: 830, up to ~2,600 (generated). Owner decision.**
- *Test fixtures:* `crates/nanovm/tests/{refused,post-return,sdk-after}` are
  three workspaces with three near-identical locks (428 + 417 + 417). One
  fixtures workspace keeps one lock.
- *All wasm crates:* the same for guests, `components/keyvalue`,
  `components/sqlite` and the SQLite fixtures gives ~2,600 fewer lock lines.
  `experiments/sqlite-filesystem` is not an experiment any more: CI's SQLite
  durability suite builds its `client` and `p3` guests, which already include
  `components/sqlite/provider/src/vfs.rs`. It belongs in
  `components/sqlite/fixtures`.
- *Path filter:* `publish-guests.yml` filters on `guests/**`, so that filter
  needs care.

**O10. `.github/workflows/publish-app.yml`: 67. Owner decision** (it publishes).
It is a template ("Nothing runs it automatically. Copy it into your app's
repository") but sits in this repo's Actions list. Either ship it as a doc
snippet or keep it. Separately, `publish-guests.yml` installs `oras` to push
what `nanovm publish` now pushes itself.

**S14. `scripts/openbench.c`: 42. Safe now.** A phase-1 check (`openat` scaling, macOS
against Linux), cited only by README Benchmarks (S8).

**O14. `base64` 0.22 → 0.23: 0 lines, one fewer crate. For the trim agents.**
`cargo tree -d` lists about 20 duplicated crate versions. All but this one come
through wasmtime, object_store, OpenTelemetry or sigstore. `nanovm` alone uses
`base64` 0.22; hyper-util, object_store and pem (sigstore) use 0.23.

**Checked and kept:**
- `container-registry` brings 22 crates that only the tests use (axum 0.7,
  tower-http, tempdir, rand 0.4, thiserror 1, nom 7, …). The loopback registry
  still needs it, and replacing it means writing our own.
- `wasmtime-wizer` only builds the JS apps, so it stays with O2.
- `bench.rs`, `spin_kv_probe` (in CI, the KV provider contracts),
  `dev-registry`, `dev-collector`, `support/{load,collector}.rs`, `demo/`,
  `scripts/vm-test.sh` (fixed in round 3, N-706) and the refused,
  post-return and sdk-after fixtures (each tests something nothing else does).
- CI's runtime line-count step.

## Broken references to fix in passing (no net lines)

- `IDEAS.md` (`notes/{examples,ops,quality}.md`, `examples/{gateway,oauth}/README.md`).
- `notes/{js,qjs,perf}.md` (`js/README.md`, ISSUES N-74).
- `crates/nanovm/tests/run.rs` (`js/README.md`, `examples/chat/README.md`; it is `dev.rs`).
- `kv.rs` (ISSUES N-9, HANDOFF, the sqlite-filesystem README).
- Issue IDs used in notes but defined nowhere: N-228, N-670, W-621, W-623, W-640, N-506, N-507.

## Questions for the owner

Each has a recommended answer, so "yes" accepts it.

1. Trim the example apps to the nine listed in O1? **Yes.**
2. Make ISSUES open work only, with closed entries left to git (O6)? **Yes.**
3. Take beta's removal of the restic backup on main now (O7)? **Yes.**
4. Keep only design notes (`multinode.md` trimmed, `storage.md`, `onramp.md`), with per-fix
   evidence living in commit messages and tests (O4)? **Yes.**
5. Drop `.agents/` entirely: vendored skills and Amp orb setup (O3)? **Yes**, unless Amp orbs are still in use.
6. Move HANDOFF's decision table into PLAN and delete HANDOFF (O11)? **Yes.**
7. Keep the JavaScript runtime (O2)? **Yes, keep.**
8. One Cargo workspace and lock for all wasm test crates, with
   `experiments/sqlite-filesystem` moved into `components/sqlite/fixtures` (O5)? **Yes.**
9. Move the test guests onto the SDK and delete `guests/util` (O8), after beta's SDK rework
   lands? **Yes, after.**
10. Take `publish-app.yml` out of `.github/workflows` and keep it as a README snippet (O10)?
    **Yes.**
