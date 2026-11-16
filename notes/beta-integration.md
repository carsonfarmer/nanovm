# beta/integration: round 3 and every beta branch, merged

`beta/integration` is round 3 (`cf/round3`, 0d60e6c, PR #2; with its later
test fix 6940893) plus `beta/deployable`, `beta/secrets`, `beta/deploy`,
`sdk/http-types`, `beta/objects`, `beta/recovery` and `beta/limits`, merged in
that order so that the features work together. Each merge commit lists its
conflicts and how they were resolved; the cross-branch work is in commits of
its own after each merge. This page is the evidence; ISSUES B-800 is the
integration's own finding.

## Merges

| Merge | Commit | Conflicts | Runtime (CI's count) |
|---|---|---|---|
| deployable | 18a8985 | ISSUES (both appended) | 7,549 → 7,554 |
| cf/round3 6940893 | 696f7f7 | none | 7,554 |
| secrets | 16341d3 | config.rs `load` (round 3's lease checks, secrets' path resolution), supervisor.rs `new` (secrets' `check_dir`; the old cgroup sweep round 3 replaced), ISSUES | 7,686 |
| deploy | a1f8f65 | config.rs `ProxyConfig::validate`, README (first-request steps), ISSUES | 7,767 |
| sdk/http-types | 7dffe15 | gateway (port + secrets' key), `sdk/rust/src/http.rs` (deleted by the port, edited by deploy), SDK README, ISSUES | 7,767 |
| objects | 34db1a9 | app.rs `App::new` (secrets' config chain, objects' `Objects`), supervisor.rs `new`, ISSUES | 8,044 |
| recovery | 7c8eb08 | ship.rs (14 hunks), supervisor.rs, journal.rs, integration.rs, README, PLAN, ISSUES, CI | 8,374 |
| limits | 3665867 | config.rs (cgroup's new type, `[clients]` beside deploy's lists), proxy.rs, supervisor.rs, integration.rs, demo config | 8,709 |

Plus 8,709 → 8,711 for the reserve in objects' room check (d753e4a).

**Recovery onto objects' layout.** Recovery was built on round 3 7b3f9c6 and
objects' first commit (1b11455): two merge bases. ship.rs was rebuilt from
this branch's (round 3's lease fixes on `ship::object_at`) with recovery's
additions ported: `Store::list`, `backlog`, `restore` staged in
`<data_dir>/restore` and checked by identity, `install` making the object's
shard and directory by descriptor (`sub`: `mkdirat`, `openat` with
`O_NOFOLLOW`, `fchown` to the app's uid when it made them) and swapping `data`,
`journal`, `shipped`, `committed` inside it, `recover`, `pause`. Round 3's
`restored` chown walk went with it (B-210 fixed). Recovery's tests run on the
new layout (`a_restore_cut_short_is_finished_or_undone`: 9 cut points, as on
recovery).

**One helper for delete and restore** (`Supervisor::held`, as the coordinator
asked). It stops what of the app runs and holds the shipper off it: with an
object, the object is paused and its instance evicted (the app's other
objects serve; recovery's behaviour); with none, the worker is drained
(objects' delete: the worker holds the app's storage lock and its object
count). The shipper hold is now per app or object (`ship::Held`, a set the
shipper reads before shipping an object; adding to it waits out ships in
flight): objects' delete held every app's shipping, and a restore, which
fetches from the store, would have stopped all shipping for its duration.
Test: `the_shipper_passes_what_is_held_over`; mutation (the set ignored)
fails it.

## Cross-branch fixes

| Commit | What |
|---|---|
| 5daa1e8 | secrets in the guide (step 6), the reference config's secrets dir (install.sh makes it 0700) |
| a953107 | reference proxies set `hosts`/`signers`, nodes `[limits] egress`; `tests/configs.rs` refuses a deploy reference without them (mutation: the merged configs fail it); secrets' `Served` test node allows the app's egress; the guide's step 5; errors.md |
| da60df5 | the operator's egress cap in the examples' and JS READMEs; ISSUES ID ranges |
| 96fd8e9 | ops docs and scripts on objects' layout (check-host's capture check had become a no-op: its test fails with the old glob); the guide's step 7; the disk-full runbook from the drill |
| c939bcf | recovery: the store is the backup in the ops docs; the restic drill, `max_versions` and `backup.py` references go |
| 6eec762 | the shipper-hold test |
| d753e4a | a new object needs its quota free above limits' reserve (the coordinator's request); limits' reserve test adapted (below) |
| 344ddd2 | limits' gate tests list their hosts and signers; secrets' `Served` sets `cgroup = "off"` |
| 14a33fa | the unit delegates its cgroup (limits' `cgroup = "auto"`); configs, check-host, tests and drills; the guide's limits section; the disk-full runbook again |

The oauth and gateway examples carry all four changes: round 3's
`__Host-session` cookie, secrets' `config::get` keys, the http-types port, and
deploy's egress (their manifests' hosts must also be in a node's
`[limits] egress`; the reference lists GitHub's). Every branch's tests of them
pass.

## Where features meet, and what was decided

- **Delete and backup (B-800, owner question).** Objects' delete removes the
  store's copy; recovery made the store the only backup. A delete can't be
  undone without the store's own old versions. Documented; the
  recommendation is in ISSUES.
- **The reserve and new objects.** Objects admitted a new object with its
  quota free; limits refuses growth below `disk_reserve_mb`. Merged, a new
  object needs its quota free above the reserve, so it is refused before it
  runs (`507 disk-full`, naming the reserve) rather than at its first write.
  Limits' `the_disk_reserve_keeps_writes_committing` asserted the old split
  (a new object refused by the reserve, mid-write); it now checks the
  combined rule, and existing objects growing down to the reserve. Objects'
  `a_full_disk_makes_no_new_objects` holds 239 counters on 4 MiB before the
  first refusal (257 before: the default reserve is 0.2 MiB there).
- **Strict defaults meet other branches' tests.** Deploy's empty `hosts`,
  `signers` and egress, and limits' `cgroup = "auto"`, failed four tests of
  other branches, five times: secrets' `Served` (after deploy for its egress,
  after limits for its cgroup) and limits' three gate tests (no `hosts`). Each
  test now configures what it uses; no default changed.

## Evidence

All as root on the shared 4-core machine, debug builds, with the guests, the
25 example apps and the JavaScript test app rebuilt (`tools/build-app.sh`, as
CI does) before each run of `env -u RUST_BACKTRACE RUST_TEST_THREADS=1 cargo
test --locked -p nanovm`. The SDK's tests (CI's step) passed after every
merge; `cargo check --locked -p nanovm --bin nanovm` was clean after each.

| After | lib | configs | dev | examples | integration | js | committed_state |
|---|---|---|---|---|---|---|---|
| deployable | 167 | 1 | 9 | 27 | 107 (14 ignored) | 17 | 2 |
| secrets | 171 | 1 | 11 | 28 | 110 | 17 | 2 |
| deploy | 180 | 1 | 11 | 27 + 1 failed (fixed in a953107) | 112 | 17 | 2 |
| sdk/http-types | 180 | 1 | 11 | 29 | 112 | 17 | 2 |
| objects | 183 | 1 | 11 | 29 | 119 | 17 | 2 |
| recovery | 186 | 1 | 11 | 29 | 122 | 17 | 2 |
| limits, final (14a33fa) | 194 | 1 | 11 | 29 | 128 | 17 | 2 |

After limits, before its fixes: examples 28 + 1 failed and integration 125 + 3
failed (fixed in 344ddd2; "Strict defaults" above).

The SQLite suite (`bash experiments/sqlite-filesystem/build.sh --compose`,
then `--test integration sqlite_ -- --ignored`) on the final tree: 12 of 12,
among them recovery's `sqlite_restores_a_lost_node_from_the_store` on objects'
layout (a fresh node restores a jailed app through `nanovm admin`).

Mutation checks of the integration's own code, each restored with an empty
`git diff`: the shipper ignoring `Held` fails
`the_shipper_passes_what_is_held_over` ("a held object shipped"); the
reserve left out of objects' room check fails `room_for_objects` and
`the_disk_reserve_keeps_writes_committing`; `install` not giving the
directories it makes to the app's uid fails
`sqlite_restores_a_lost_node_from_the_store` (the jailed worker: "unable to
open database file", 500); the four reference configs as merged fail
`tests/configs.rs`; check-host without its controllers check fails
`test_auto_cgroup_needs_v2_with_the_controllers`.

Ops checks on the final build: the units test in systemd-nspawn, four modes,
70/70 (the cgroup mode with `cgroup = "auto"` in the unit's delegated cgroup);
the single-node e2e 9/9 (a new check: a host the proxy doesn't serve gets 403);
`docs/ops/errors-check.sh` 21 checks as documented; the disk-full drill as the
runbook says.

## Not done or not verified

- The container test (`deploy/container/test/local.sh`) and the binary-swap
  and collector-outage drills weren't rerun; `systemd-analyze verify` and
  `security` weren't rerun on the unit with `Delegate=`.
- `[worker]` limits enforced through cgroup v2: this host's v2 hierarchy has no
  controllers, so only the placement was checked.
- CPU per request wasn't measured: the merges add no hot-path code of their
  own; each branch measured its own.
