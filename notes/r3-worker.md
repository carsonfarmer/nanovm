# Worker review 2 fixes (`fix/r3-worker`)

The second review of the worker area (round 3, PR #2) found W1, W2 and two low items, and
suspected two more. All six are fixed here, and one more found while fixing W2 (N-700..N-706 in
ISSUES.md). Tests ran in this sandbox (debug build, `RUST_TEST_THREADS=1`) unless they say VM.

## What changed

- **Worker cgroups (N-700, N-701, N-702).** At startup the supervisor takes `cgroup`
  (`Cgroup::take`): it refuses a directory that isn't cgroup v2 (`fstatfs` magic), holds an
  exclusive `flock` on it for its life so no second node uses it, and removes only
  `worker-<digits>` entries. A worker's cgroup that is busy when dropped (its worker dropped
  without being waited for, or an earlier run's) has what is in it killed (`cgroup.kill`) and is
  removed by a short-lived thread once empty (at most 10 s). The reaped path is unchanged.
- **Retirement (N-703, N-705).** `drain` hangs up on the old worker (`Dialer::hang_up`, a
  `shutdown` of its request control socket). The worker's serve loop returns; it runs
  `Node::shutdown` (objects finish their queued and after-answer work and commit), flushes its
  telemetry and exits. The reaper kills it if it hasn't exited within `post_return_timeout_ms`
  plus 2 s. A dropped `Live` (failed start, supervisor gone) still kills at once. A graceful node
  shutdown can reuse `drain` for every worker.
- **Memory limit below a spare's use (N-704).** `limit` compares `memory_mb` with the cgroup's
  `memory.current` before setting `memory.max`, and refuses the start with a message naming both.
- **Docs and tooling (N-706).** `scripts/vm-test.sh` runs again (CA bundle copied, stale
  `demo/static` copy removed, `sleep` linked, the cgroup tests in its default set). The `cgroup`
  doc names the node's `[worker]` limits.

## Evidence

| Test | Before | After |
|---|---|---|
| `review2_startup_removes_only_cgroups_it_made` | removed `reserved` and the plain directory's `keep` | both kept; the plain directory refused ("… is not a cgroup v2 directory") |
| `startup_removes_an_earlier_runs_worker_cgroups` (new) | a leftover holding a process still there after 10 s | both leftovers gone, the process killed |
| `two_nodes_cannot_share_a_cgroup` (new) | the second node removed the first's fresh `worker-<n>` | the second refuses ("another node runs its workers in cgroup …"); the first serves on, same pid |
| `a_dropped_supervisors_worker_cgroups_go` (new) | the spare's `worker-1` left 10 s after the runtime was dropped | none left |
| `review2_a_retired_workers_last_telemetry_reaches_the_collector` | egress-denied event lost in 3 of 3 rounds | delivered in 3 of 3; each switch under 2 s |
| `a_retired_workers_work_after_the_answer_commits` (new) | `"later": false` (work after the answer rolled back) | `"later": true` |
| `a_worker_memory_limit_below_a_spares_use_is_refused_clearly` (new, VM) | `502 Broken pipe (os error 32)`, one `oom_kill` | 502 "[worker] memory_mb = 1 is below the N MiB a worker holds before it has an app", no `oom_kill` |

The cgroup tests that need no controller run on this sandbox's cgroup2 (`/sys/fs/cgroup/unified`).

**VM** (`scripts/vm-test.sh`, release build, QEMU TCG, kernel 7.0.0-42, cgroup v2 with cpu, memory
and pids; host load 12–24 on 4 cores from the other agents' builds):
- passed: the memory-limit test above, `review2_startup_removes_only_cgroups_it_made`,
  `startup_removes_an_earlier_runs_worker_cgroups`, `two_nodes_cannot_share_a_cgroup`,
  `a_dropped_supervisors_worker_cgroups_go` (the last two failed in the first two runs, at load
  ~20, on their first request, before any cgroup check), `review_spare_engine_memory_is_charged_to_the_app`,
  `jail_blocks_escapes`.
- failed, the same on `cf/round3` (7b3f9c6, run with this branch's `vm-test.sh`):
  `workers_are_placed_in_their_cgroups` (hostile's first request 500/503), `worker_memory_max_kills_only_that_worker`
  (every alloc 500, so no OOM), `worker_cpu_and_pids_limits_hold` (the worker, at `cpus = 0.2`,
  did not answer within 120 s). Under emulation on a loaded host, guest time includes time the
  host doesn't run the VM, so the hostile guest's 100 ms CPU budget and the start deadline are
  overrun. Not caused by this branch; not fixed here.

Retirement time, hang-up to reaped, in the W2 test (debug build, 3 switches): 10.5–15.8 ms; the
switch request 85–175 ms. Without the hang-up the worker is killed at the bound: 2.005–2.007 s.

Mutation checks (each restored with the saved file, `git diff` checked):
- no hang-up in `drain` → the W2 test fails: switches 2.05–2.16 s ("waited for a retired worker
  to be killed"). Without the new timing assertion the test passed: the worker's 1 s export
  timer fired before the 2 s kill.
- the worker exits without flushing → the W2 test fails: rounds [0, 1, 2] lost.
- no `Node::shutdown` before the flush → `a_retired_workers_work_after_the_answer_commits` fails.
- no cgroup v2 check and no lock → `review2_startup_removes_only_cgroups_it_made` (the plain
  directory accepted) and `two_nodes_cannot_share_a_cgroup` (fresh cgroup removed) fail.
- the sweep takes every name, and no `cgroup.kill` → `review2_startup…` (`reserved` removed) and
  `startup_removes_an_earlier_runs_worker_cgroups` (busy `worker-1001` left) fail.
- no retry once the busy cgroup's worker has exited → `a_dropped_supervisors_worker_cgroups_go`
  (`worker-1` left) and `startup_removes_an_earlier…` fail.
- the whole `src` change reverted → all four cgroup tests and the W2 test fail.

## Runtime lines

7410 → 7428 (cgroups, +18) → 7437 (retirement, +9); after merging `cf/round3` (+8 from r3-dev)
7445 → 7448 (memory check, +3) → 7449 (after-answer work on stop, +1). In all +31.

## Not done

- Graceful node shutdown itself (signals) is the beta item's; `drain` is ready for it.
- `[worker] memory_mb` must still leave the app room above the spare's footprint; a limit just
  above it starts and then OOM-kills under load, as any too-small limit does.
