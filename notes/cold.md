# Cold starts and density (cold workstream)

Branch `cf/cold`, from `491e790`. Measured on the shared 4-core Linux VM (kernel 6.18, ext4 mounted with `discard`) while other agents were compiling: load average 8 to 22. Wall-clock numbers are noisy. Memory-per-object numbers are exact and repeat run after run.

## What got better

| | before | after |
|---|---|---|
| Churn: a new object plus an LRU eviction (quick bench) | 1,388/s, p50 38.8 ms, p99 133 ms | 3,359/s, p50 15.3 ms, p99 58 ms |
| First request after a trap (a fresh instance) | p50 0.8 ms | p50 0.3 ms |
| Syscalls per churn request | 38.6 | 23.6 |
| Syscalls per cold start, teardown included | 43.4 | 29.4 |
| Memory (RSS) per live object | 115–116 KB | 89 KB |
| Heap allocated per live object | 67.6 KB | 35.7 KB |
| Cold-start CPU and throughput | | unchanged within the noise |
| File descriptors / memory mappings per live object | 2 / 4 | 2 / 4 |

The churn and restart rows are medians of 4 alternating pairs against `7dabdaf` (base code plus the bench changes). The memory rows come from every run.

## 1. Empty `/tmp` directories are reused (f1e89d6)

**What was slow.** Every instance created its own scratch directory for `/tmp` (`mkdir`) and deleted it when the instance ended (`rmdir`). It also resolved its volume's path with `canonicalize`, which costs one `readlink` per path component (10 here). On this machine an `rmdir` takes about 70 µs of CPU and, because the disk is mounted with `discard`, about 3.3 ms of waiting. That wait blocked one of the 4 worker threads every time an object was evicted or restarted. A `mkdir` takes 15 to 40 µs.

**The fix.** When an instance ends and has left its `/tmp` empty (checked by reading the directory), its directory goes on a free list, and the next instance of any object of that app gets it. A directory with anything left in it is deleted as before, so every instance still starts with an empty `/tmp`. The app's objects directory is resolved once when the app loads. A volume is created with one `mkdir`; a volume that already exists must be a real directory, not a symlink, because the journal compares the volume's path with where open files really are.

**What it does not help.** First-ever cold starts of many new objects at once: no instance has ended yet, so there is nothing to reuse.

**Tests.** `only_empty_scratch_is_reused` (new) checks that a directory left empty is handed to another object's next instance, and that a directory left with a file is removed and not reused. It fails if reuse is removed, and fails if the emptiness check is removed. `disk_quota_and_tmp_wipe` now checks the removed directory by its contents, since directories no longer carry the object's name.

## 2. Less memory per live object (d0e85be)

A heap profile (heaptrack, peak consumers with 2,000 idle objects) showed where each live object's 68 KB of heap went. 36 KB of it was in two of our own structures:

- **The mailbox: 19 KB.** A tokio channel allocates room for 32 messages up front, and a request (`Job`) is 592 bytes. Requests are now boxed, so each slot holds a pointer. This adds one small allocation per request.
- **The actor task: 16.9 KB.** `charged()` took the store's event-loop future by value and pinned it inside. Rust then kept two copies of the 7.6 KB future in the task. The caller now pins it. The publish path for jailed workers (2.7 KB) is now boxed only when there are publishes to send. The task is now 2.9 KB (`-Zprint-type-sizes` and a size probe).

Result: RSS per live object went from 115–116 to 89 KB, and heap per object from 67.6 to 35.7 KB. `an_actor_is_small` keeps the task under 4 KB; with the `charged()` change reverted it measures 5.3 KB and fails.

What remains per idle object (89 KB): about 25 KB of wasmtime's own instance data (4 core instances for the counter guest), 28 KB of the guest's memory pages, 16 KB for two fiber stacks the store keeps, 8 KB of tables, and our state. wasmtime 50 has no public way to give a store's cached fiber stack back to the pool (N-52).

## Measured, not adopted

- **Remembering `/data` usage between instances** instead of walking the volume at each start (W-27). For the counter guest the walk is 5 syscalls, 2% of cold-start CPU. Remembered counters would keep any drift instead of correcting it, and they mix `/data` with the discarded `/tmp`. Not worth weakening exact quotas.
- **Pre-initializing guests with `wasmtime-wizer` 50.0.1** (W-28). A restart plus first request costs the counter guest 235–356 µs of CPU, against 43–100 µs for a warm request. The guest's start functions are under 1% of cold-start CPU, so a Rust guest has little to snapshot. The composed SQLite example costs 1.69 ms against 0.66–1.0 ms warm; that gap is worth trying, as a guest build step (dx), not in the runtime.
- **Not measured:** `pagemap_scan` with `keep_resident` for reusing memory slots without page faults (W-26), and worker start time (W-29). Directory sharding (W-25) changes the on-disk layout and needs the owner's decision.

## Where a cold start's time goes now

From perf over 3,000 cold starts after both changes:
- `mkdir` (the volume, and a scratch directory while none is free): about 20%. Here it is inflated by ext4 scanning recently deleted inodes, because the bench deletes the previous run's data first.
- Page faults: about 11%.
- Guest code: about 10%.
- `Journal::local_at` (a `statx` and a `readlink` per open in `/data`): about 3%.
- The volume walk: about 2%.

TLB shootdowns from `mprotect`/`mmap` take about 1–5%.

## How to reproduce

```bash
# binaries: the base (7dabdaf) and each commit, copied out of target/ under distinct names
cargo build --locked --release -p nanovm --example bench
ulimit -n 20000
# alternate A and B, at least 3 pairs each:
env -u RUST_BACKTRACE ./bench-A --quick --only cold,kill,churn --cold 2000
env -u RUST_BACKTRACE ./bench-B --quick --only cold,kill,churn --cold 2000
# heap per object (one shared counter, so not for CPU numbers):
HEAP_COUNT=1 env -u RUST_BACKTRACE ./bench-B --quick --only cold --cold 2000
# syscalls per cold start: strace -f -c a run with --cold 1000 --conc 4 and one with --cold 0,
# removing target/bench-data first each time, and divide the difference by 1000
```

The bench now prints CPU µs per cold start and per churn request, file descriptors and mappings per live object, and heap per object with `HEAP_COUNT`. `--only` picks scenarios; `--conc` sets the cold/warm concurrency.

## Docs to fold in (orchestrator)

- PLAN.md "Quota'd filesystem": `/tmp` is at `tmp/<app>/<n>`, and a directory an instance left empty is reused by the next instance.
- README "Benchmarks / Linux": RSS per live object is 89 KB on this VM with this branch (115 KB at its base, measured the same way).
- README "Host requirements": still 2 fds and about 8 mappings per object (4 per pool slot plus 4 per live instance, measured).
