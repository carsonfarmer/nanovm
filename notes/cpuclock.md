# The CPU budget charges CPU (cpuclock, N-640)

## What was wrong

Each request has a CPU budget (`cpu_ms`, default 200 ms). The node measured it
with the wall clock: it read the time before and after each poll of the
guest's store and charged the difference. If the guest's thread was
descheduled in the middle of a poll, or blocked in an `fsync` the guest asked
for, that time was charged as CPU. The journal subtracted its own fsync waits
(`uncharge`), but nothing else.

So an overloaded node killed honest requests with `500 cpu`, and each kill
restarted an instance, which added load. On this shared 4-core VM at load
~20 the SQLite durability tests (5–10 s budgets) and
`kv_counts_against_the_disk_quota` (100 ms) failed and passed when idle. The
examples agent's N-600 is the same bug from the other side: the SQLite
provider waits for each `fsync` inside the guest's poll, so 40 one-row
INSERTs used up the 200 ms budget while using little CPU.

## What changed

- A poll is charged the change in the polling thread's own CPU clock
  (`clock_gettime(CLOCK_THREAD_CPUTIME_ID)`, `metrics::thread_cpu_ns`). That
  is user plus kernel time: what the guest runs, and the host calls it makes
  inline (file writes, the kernel part of its `fsync`s), count. Waiting for
  a core or for the disk is not CPU and does not count.
- The epoch callback, which enforces the budget, runs on the polling thread,
  so it reads the poll in progress with the same clock. Nothing reads the
  charge from another thread.
- The journal's own work stays uncharged, as before, but measured with the
  thread clock instead of as wall time: writing and syncing undo records,
  renames into the journal, and the commit (its syncs and, with `[capture]`,
  the tar and hash of the volume). That was written to leave fsync waits
  out, which the clock now does by itself, but it also left out the
  journal's CPU, which is real: 38–48 ms of the 124–168 ms a 198-key KV fill
  used at load 22 (release, measured by timing `record`/`or_poison` with the
  same clock). It is the node's mechanism, its cost depends on how the node
  stores undo, and capture is the operator's choice, so the guest isn't
  billed for it. The change to `journal.rs` is only the clock at its three
  existing call sites.
- `metrics.guest_cpu_ns` (`nanovm.guest_cpu_ns`) is now CPU time spent
  polling objects, the journal's work included. Before, it was wall time in
  polls.
- The wall-clock deadlines (`request_timeout_ms`, 504) are unchanged and
  still stop a request that is slow for any reason, including an overloaded
  node.
- Linux and macOS both have `CLOCK_THREAD_CPUTIME_ID` in libc, so no `cfg`.
  The worker's seccomp filter already allows `clock_gettime`.

Runtime lines (CI's count): 5,419 → 5,422.

## Evidence

All on the shared 4-core VM, `debug` test builds unless noted.

**New test** `cpu_budget_is_cpu_not_time_waiting_for_a_core`: the node runs on
one thread pinned to one CPU, with seven busy threads pinned to the same CPU,
and a 100 ms budget. It first measures how many `/spin?n=` steps take a
third of the budget, then sends that request and `/spin` (forever) under
contention, timing each with the node thread's CPU clock. 5 runs each:

| | 1/3-budget request | `/spin` killed after |
|---|---|---|
| before | **500 cpu** after 11.9–16.1 ms CPU (112–127 ms wall) | 13.8–18.1 ms CPU |
| after | 200 after 31.4–39.8 ms CPU (291–406 ms wall) | 102.0–103.3 ms CPU (0.91–1.13 s wall) |

The test asserts the contention (wall > 3× CPU), the success, and a kill
between 90% and 150% of the budget. It takes about 1.5 s.

**Sync-heavy SQLite (N-600).** `/bench?op=autocommit` on
`target/sqlite-filesystem/p3.wasm` runs 100 one-row UPDATE transactions with
`synchronous=FULL` and a DELETE journal, so each waits for several fsyncs.
The new ignored test `sqlite_sync_waits_are_not_charged_as_cpu` sends it
three times at the default 200 ms budget. Load 16, 3 runs each:

| | outcome | CPU in polls (`guest_cpu_ns`) | wall |
|---|---|---|---|
| before | 3/3 runs fail at the first request, `500 cpu` | (wall time: 249–273 ms) | 256–273 ms |
| after | 9/9 requests 200 | 70–133 ms | 0.47–1.99 s |

The charge is below the CPU in polls, which includes the journal's work. A
scratch probe (release, a single-threaded node so its thread's clock is the
node's CPU, load 27–28, 24 requests per build) agrees: before, all 24 were
killed after 6.9–29.6 ms of real CPU (240–584 ms wall); after, all 24
succeeded using 68.9–135.1 ms (median 91), in 0.6–4.2 s. About 40% of that
CPU is kernel time (35–57 ms of 93–164 ms, `getrusage(RUSAGE_THREAD)`).

**The tests that failed under load.** Full `sqlite_` ignored set (12 tests
with the new one; restic is installed now, so the backup test ran too),
alternating builds:

| run | load | before | after |
|---|---|---|---|
| 11 tests, fixtures unchanged, 2 pairs | 9–20 | 8/11 twice: `handled_storage_exhaustion`, `journal_pressure_matrix`, `repeated_updates_remain_bounded` fail `cpu` (5 s and 10 s budgets) | 11/11, then 10/11 (`handled_storage_exhaustion`: 504 at its 15 s deadline) |
| 12 tests, final fixtures, new/old/new | 6–14 | 9/12: `handled`, `repeated`, `sync_waits` fail `cpu` | 12/12, 12/12 |
| the 4 + `kv_counts_against_the_disk_quota`, with 16 busy processes of my own | 21–37 | 1/5: all four SQLite tests fail `cpu` | 5/5 |

`sqlite_handled_storage_exhaustion` and `sqlite_journal_pressure_matrix`
then failed a different way: their 1,000-statement requests hit the 15 s
wall-clock deadline (504) while using 0.13–0.87 s of CPU. The same churn in
`sqlite_repeated_updates_remain_bounded` (60 s deadline) took 1.3–16 s of
wall time for 0.25–0.83 s of CPU: the disk, busy with the other agents'
builds, sets the time. Both now have that test's 60 s deadline; their CPU
budgets are unchanged.

`kv_counts_against_the_disk_quota` (the orchestrator's data point), unchanged
test, load 11.6, alternating: before 2 of 3 runs failed `used more than 100ms
of CPU`; after 3 of 3 passed. At load 20 it failed once after, at hostile's
1 s deadline: its 198 key writes took 1.2–2.4 s of wall time and 101–163 ms
of real CPU (half of it kernel; 38–48 ms in the journal's records, not
charged) in a release probe at load 22–28. It is a quota test, so it now uses
`filler()` (1 s of CPU, and a 10 s deadline added there for the same reason).

`kv_refuses_bad_names_and_streams_keys` (1,500 key writes) hit its 10 s
deadline once in a full suite at load 22, as N-32 records for the old build;
it now has 60 s. Its CPU budget is unchanged.

`cpu_hogs_leave_cores_for_short_requests` failed after the change at load
12–20, 3 of 3: its 16 hogs take turns on 2 lanes, and on a loaded host each
gets its 100 ms of real CPU only after hostile's 1 s deadline, so they ended
in 504, not `cpu`. Before, the lane turns were billed as wall time. The test
is about the canary, and its hogs now have a 10 s deadline, so they still
die by the budget; it passes 3 of 3 at load 20 (canary assertions unchanged).

Full locked suite after: 109 unit, 71 integration (14 ignored), 2 example
tests pass (load 4–13); at load 22 the run before the 1,500-key deadline
change failed only that test, and a later run once had
`the_proxy_outlasts_idle_and_excess_connections` miss the proxy's start-up
by its 2 s (it does not run objects; 3 of 3 alone pass). `cargo check
--locked -p nanovm --bin nanovm` passes.

## Cost

`CLOCK_THREAD_CPUTIME_ID` is a system call; `CLOCK_MONOTONIC` (what
`Instant` uses) is not. A C loop of 2M calls, 5 rounds, idle and at load 26:

| clock | CPU per call |
|---|---|
| `CLOCK_MONOTONIC` | 25–27 ns |
| `CLOCK_THREAD_CPUTIME_ID` | 172–202 ns |

Reads per request, counted with `perf stat -e syscalls:sys_enter_clock_gettime`
over the in-process probe (release; the old build made 6 such calls in its
whole run): warm GET at 64 concurrent over 1,000 objects **8.1**, one client
on one object **5.8**, `POST /incr` (writes `/data`) **30.2**. Two per poll,
one per epoch tick while wasm runs, two per journal record and per dirty
commit. So the expected cost is about 8.1 × 0.16 ≈ 1.3 µs per warm GET
(≈2%), 0.9 µs per serial GET (≈3%), 4.8 µs per `/incr` (≈0.5%).

Measured process CPU per request (release in-process probe: `load::run`
against a node running the counter with its bench limits, process CPU
from `CLOCK_PROCESS_CPUTIME_ID`; 7 alternating rounds at load 19–29, so
noisy; these rounds predate keeping the journal's records uncharged, which
only adds reads to `/incr`):

| workload | before median (range) | after median (range) | paired after − before, median |
|---|---|---|---|
| GET, 64 concurrent, 200k | 66.8 µs (59.3–70.2) | 68.1 µs (61.7–75.1) | +1.6 µs |
| GET, one client, 20k | 27.1 µs (26.0–33.1) | 30.3 µs (27.8–41.1) | +2.0 µs |
| POST /incr, 64 concurrent, 20k | 1061 µs (976–1154) | 1045 µs (947–1142) | −1.0 µs |

Three more alternating rounds of `/incr` with the final code, load 21–26:
before 962 µs (958–979), after 945 µs (923–947), with 5 `500 cpu` on the old
build and none on the new.

The difference is inside the noise; the syscall count above is the floor.
The probe also counts failures: the old build failed 178 of 1.4M GETs and 94
of 140k `/incr`s with `500 cpu` (the counter's 50 ms budget) across the 7
rounds; the new build none.

## How to reproduce

```
export PATH=/home/user/wt/tools/bin:$PATH
bash scripts/build-guests.sh            # the hostile guest's /spin takes n= now
ulimit -n 20000
env -u RUST_BACKTRACE RUST_TEST_THREADS=1 cargo test --locked -p nanovm --test integration \
    cpu_budget_is_cpu_not_time_waiting_for_a_core -- --nocapture
# needs experiments/sqlite-filesystem/build.sh --compose (CI's SQLite step):
env -u RUST_BACKTRACE RUST_TEST_THREADS=1 cargo test --locked -p nanovm --test integration \
    sqlite_ -- --ignored --nocapture
```

For the "before" column, run the same test file against `d6c5bc0`'s
`crates/nanovm/src`. To load the machine yourself: `for i in $(seq 16); do
yes > /dev/null & done` (and `kill` them after). The CPU-per-request and
single-threaded SQLite probes were scratch examples, as described above; the
syscall counts are `perf stat -e syscalls:sys_enter_clock_gettime` around
them.

## Decisions and risks

- **Kernel time counts.** A guest's file calls run inline on its thread; the
  kernel time they cost is CPU the request made the node spend, and leaving
  it out would let a guest hide work in system calls. The journal's work on
  top of them doesn't (above). So no request is charged more than before:
  the charge was wall time in the poll, which is at least its CPU, minus the
  journal's wall time, which is at least its CPU, and nothing else changed.
- **Overload now ends in 504, not 500 cpu.** A hog on an overloaded node
  holds its heavy lane until it has really used its budget, or until its
  wall-clock deadline. Before, overload killed it sooner, which was also
  how honest requests died. Heavy lanes still cap how many cores hogs hold.
- **CPU per tick depends on the host.** On a slower or SMT-shared core the
  same guest work costs more CPU time; this is still true of any CPU clock.
