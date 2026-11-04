# App cold starts (cold2 workstream)

Branch `cf/cold2`, from the merged wave-1 branch (`2ee94bc`). Serverless means scale-to-zero, so after an object's
cold start the cost that matters most is an **app's**: the first request to an app whose worker isn't running. That
happens after an idle app is retired to make room for another (`max_apps`, least recently used), after a crash, and on
a version switch. This note measures it end to end on the production path (dev-mode supervisor, jailed worker,
loopback registry, counting OTLP sink) and records what changed.

Measured on the shared 4-core VM while other agents compiled (load average 5 to 15). Wall times are noisy; every
comparison alternates the builds and gives medians and ranges. CPU is per process, from `/proc/<pid>/task/*/schedstat`.

## What got better

| First request to an app whose worker isn't running | before (`2ee94bc`) | after (`77497df`) |
|---|---|---|
| (b) known version, worker killed (counter, 198 KB) | 20.8 ms [17.2–33.7] | 7.0 ms [5.9–23.5] |
| (b) same, JavaScript app (5.4 MB component) | 73.5 ms [63.3–100.2] | 9.7 ms [7.8–21.5] |
| (b) after the worker had served 200 objects | 119 ms [93–308] | 10.1 ms [9.3–14.1] |
| (b) after the worker had served 1,000 objects | 1,211 ms [1,051–1,875] | 51 ms [40–85] (first change alone) |
| (b) app retired for another (`max_apps = 1`, two apps) | 34.2 ms [19.3–63.0] | 16.2 ms [10.4–27.8] |
| (c) switch to a version whose compile is cached | 21.2 ms [18.5–35.0] | 10.4 ms [9.4–20.1] |
| (a) never-seen version, (c) switch to a new version | 200–450 ms, all compile | unchanged (same compile) |
| CPU on the request's path, (b) counter | worker 17.7 ms, supervisor 3.0 ms | worker 5.6 ms (+ 9.1 ms ahead of time), supervisor 2.5 ms |
| CPU on the request's path, (b) JavaScript | worker 56.1 ms, supervisor 8.6 ms | worker 8.2 ms (+ 9.7 ms ahead of time), supervisor 2.8 ms |

Rows 1, 2, 6 and the CPU rows are one alternating run (3 rounds; n = 15, 9 and 6). The others come from the A/B of
the change that moved them (below), or (rows 3 and 5, "after") from the instrumented after-state run. "Ahead of
time" is the spare worker's start-up, which no longer waits on a request.

## Where an app start's time went, and goes

Temporary marks (timestamps and process CPU at each step, printed to stderr; not committed) on the restart of a
known version, counter guest, 6 runs each, medians:

| step | before: wall (CPU) | after: wall (CPU) |
|---|---|---|
| supervisor: route, fetch the manifest, read the blobs | 0.9 ms | 0.7 ms |
| supervisor: fork and exec the worker | 2.6 ms (0.8) | — (a spare is waiting) |
| worker: exec to `main` (dynamic relocation, glibc start-up) | 1.7 ms (3.4) | — (ahead of time) |
| worker: read the spec and the component from stdin | 0.6 ms | 0.1 ms (spec only) |
| worker: build the engine (wasmtime's pools) | 6.3 ms (7.2) | — (ahead of time) |
| worker: linker | 0.6 ms | — (ahead of time) |
| worker: jail, tokio runtime | 1.0 ms | 1.0 ms |
| worker: telemetry SDK | 0.9 ms | 1.0 ms (N-155) |
| worker: SHA-256 of the component | 1.4 ms (1.7) | — (the registry's digest is the key) |
| worker: load the cached compile, link, scratch | 1.6 ms | 1.1 ms |
| readiness probe, first instance and response | 2.2 ms | 2.7 ms |
| **total** | **22.3 ms** | **8.4 ms** |

What remains is small pieces: telemetry set-up, the jail, the runtime, mapping the compiled component, and the first
request itself. For a switch, the old worker's exit is now the largest piece (N-154, about 10 of 21 ms): the kernel
unmaps its 4,500 mappings before it reads as gone, and the new worker must not start before.

For a never-seen version (a) and a switch to a new one (c), compiling is everything: 370–450 ms of CPU for the
198 KB counter (Cranelift's register allocation and optimization passes), 160–450 ms of wall time on the shared
cores (W-72). The registry steps add about 47 ms in these measurements, but that is the test registry: it streams
blobs in small chunks on a kept-alive connection and the last one waits for a delayed ACK (curl on a fresh
connection fetches the same blob in 1–3 ms).

Hypotheses from the brief, checked: the pool's set-up was the biggest single cost (yes: 7 ms, all of it the stack
pool's per-slot guard pages); `.cwasm` loading already used `mmap` (0.6 ms, fine); cached blobs were not re-hashed by
the supervisor but were by the worker (fixed); spawn and the stdin transfer (fixed, changes 2 and 3); readiness is one
request answered as soon as the worker serves, not a polling loop (0.4 ms); `preflight` is 0.05 ms; telemetry set-up
is 0.9 ms (N-155, left: it trades against dropped spans).

## The changes

### 1. A restarted worker reuses the scratch directories its predecessor left (`e082357`, N-150)

**What was slow.** Every instance has a `/tmp` directory; since wave 1 an empty one is kept for the next instance
instead of removed (`f1e89d6`). When a worker is killed, retired or switched, its directories stay on disk, and the
next worker's start removed every one of them before serving. On this disk (ext4 mounted with `discard`) a removal
waits about 3 ms. So the first request after a worker that had served 200 objects waited 0.1–0.6 s, and after 1,000
objects 1–2 s (up to 12 s while the machine was loaded).

**The fix.** The new worker hands an empty leftover directory out again and removes only those with something in
them, as an instance's end does. New directories are numbered past the names left, so they never collide.

**Numbers** (restart after the worker served N objects, alternating builds): N = 200: 119 ms [93–308] → 37 ms
[20–54], worker CPU 30.2 → 19.9 ms. N = 1,000: 1,211 ms [1,051–1,875] → 51 ms [40–85], worker CPU 83.8 → 26.3 ms.
N = 1: unchanged.

**Test.** `scratch_left_by_a_previous_run`: an empty leftover is handed out as it is (same inode), a used one is
removed, a new name is past those left. It fails with the old removal, and without the name bump.

### 2. The next worker is spawned ahead and builds its engine before its app (`99d0f55`, N-151, N-152)

**What was slow.** 11 of a restart's 21 ms had nothing to do with the app: forking the supervisor, exec'ing the 49 MB
binary (3.4 ms of CPU for dynamic relocation and glibc start-up), and building the engine. That took 7 ms, nearly all
of it wasmtime creating the stack pool: one `mprotect` per stack slot, `2 × (max_live_objects + 32)` of them, so it
grows with the node's size (a probe: 7.7 ms with the pool, 0.1 ms without its stacks).

**The fix.** A worker now reads the node's config first, builds its engine and linker, and then waits for its app.
The supervisor keeps one such spare and spawns the next after each start. A start takes the spare (or spawns one if
it died), places it in the app's cgroup, and gives it the app, its uid and its component. A spare holds only what the
node's config decides, which a running supervisor never changes; it gets no app, uid, cgroup or jail until it is
taken, and it exits when its stdin closes, so with the supervisor. It costs 2.2 MB of memory (plus 12 MB of shared
binary pages) and its own start-up, about 9 ms of CPU, which no longer waits on a request.

The jail must still be entered before any guest code is compiled or run, and with one thread, because the uid,
Landlock and seccomp bind only the calling thread and the threads it starts later. Building the engine before the jail
is safe only while it starts no thread, so the jail now checks it: it refuses a process with more than one thread.
That check found that the jail test (`jail_blocks_escapes`) had been jailing one thread of a two-threaded process (the
test harness runs a test on its own thread), so it would not have caught a jail entered too late (N-152; production
was not affected). The probe now runs in a forked, single-threaded child.

**Numbers** (alternating with change 1's build): restart 21.5 ms [16.8–36.5] → 10.0 ms [8.1–13.4] (n = 15), worker
CPU on the path 20.1 → 8.1 ms; LRU retirement 34.2 ms [19.3–63.0] → 18.0 ms [10.6–24.0] (n = 12); switch to a cached
version 24.1 ms [20.8–60.2] → 15.8 ms [11.3–17.9]; never-seen version unchanged within the noise.

**Tests.** `workers_are_spawned_ahead`: each app's worker process was started before the app was asked for (it
fails if the spare is not spawned at start-up, or not after each start). `the_jail_needs_one_thread`: a second
thread makes the jail refuse (fails without the check). `jail_blocks_escapes` passes with the probe single-threaded.

### 3. A worker gets its cached component file and reads it only to compile (`77497df`, N-153)

**What was slow.** The supervisor read the cached component and piped it to the worker, which hashed it (SHA-256) to
look up its cached compile, though the registry had already checked the bytes against their digest when it fetched
them. This VM's CPU has no SHA extensions, so the 198 KB counter cost 1.4 ms and the 5.4 MB JavaScript component
40 ms of worker CPU, plus 7.5 ms in the supervisor.

**The fix.** The registry returns the component's digest and its file in the cache (still checking its size and
`max_component`). The worker opens the file before its jail, keys its compile cache by the digest, and reads the
file only if it has to compile. In-process nodes (`nanovm run`, tests) still pass bytes and hash them. A manifest's
digest names a path only after it is checked, now in one place.

**Numbers** (alternating with change 2's build): JavaScript restart 78.1 ms [51.7–129.1] → 16.0 ms [10.4–29.7],
worker CPU 40.3 → 6.7 ms, supervisor 7.5 → 2.3 ms; counter restart 10.6 → 9.0 ms; compiles unchanged.

**Test.** `a_cached_compile_is_used_without_reading_the_component`: the registry's copy of the component is
overwritten with zeros and the worker restarted; it still serves. It fails when the component is piped and hashed.

## Found, not changed

- **N-154**: a switch or retirement waits 5–12 ms for the old worker's address space to be torn down. Waiting for
  full exit is what proves it can no longer touch the volumes, so it stays; W-71 would shrink it, W-73 would overlap
  some of the new worker's start with it.
- **N-155**: the telemetry SDK initializes all 2,048 slots of each worker's span queue at start (most of its 1 ms,
  and memory every worker keeps). A smaller queue risks dropped spans (N-36).
- **W-70**: in production a start makes about a dozen registry round trips (manifest, referrers, signatures, each
  with a 401 and a token request). That is probably the largest part of a real app cold start and could not be
  measured here. Token reuse and remembering an accepted version across retirement are owner decisions.
- **W-72**: compile time for a never-seen version (incremental compilation across versions is the lead).
- **W-26** (`pagemap_scan`) and N-52 were not reached. Under a jail, `PAGEMAP_SCAN` needs a seccomp rule, and
  wasmtime probes it when the pool is created, which a spare now does before its jail.
- When the supervisor reaps a worker, the kernel flushes that process's `/proc` entries (`proc_flush_pid` →
  `shrink_dcache_parent`), which perf showed costing the supervisor several ms per reap in some runs. Part of it
  was the benchmark reading `/proc/<pid>/task`; not pursued.
- One run right after the container restarted had a single `502 worker-down` ("connection closed before message
  completed") on a cached switch, while the machine went from idle to fully loaded and an earlier scenario had left
  workers behind; 45 repetitions of the same scenarios straight after, and about 25 before, passed. Recorded here
  rather than explained.

## For anyone benchmarking through a supervisor

A supervisor now has two child processes: its worker and a spare with one thread. Scripts that expect exactly one
child (`bench-run/bin/measure.py` asserts `len(workers()) == 1`) should take the children with more than one thread.

## How to reproduce

Scripts are in the shared scratchpad's `cold2/` directory: `push.py` (pushes an app to the loopback registry,
optionally with a nonce custom section so its bytes and compile-cache key are new), `start.sh` (registry, OTLP sink
and one dev-mode supervisor per build, `max_live_objects = 1100`, data under `target/coldrun`, which the jailed uids
can reach), `cold.py` (scenarios below; one JSON line per first request: wall ms, supervisor CPU, the new worker's
CPU, the spare's CPU, and phase marks if the build prints them), `phases.py` (medians and ranges per step).

```bash
# Build with the same targets each time: dev-deps change feature unification, which rebuilds wasmtime.
cargo build --locked --release -p nanovm --features dev --bin nanovm --example dev-registry
# Copy target/release/nanovm to target/coldrun/nanovm-<name> for each commit, then:
PORTIDX=0 start.sh base; PORTIDX=6 start.sh new
for round in 1 2 3; do for v in base:18600 new:18660; do   # alternate the builds
  n=${v%:*}; p=${v#*:}; pid=$(cat target/coldrun/$n.pid)
  python3 -I cold.py $n $pid $p 127.0.0.1:15420 out restart 5
  CONFIG=js.json python3 -I cold.py $n-js $pid $p 127.0.0.1:15420 out restart 3 js/target/counter.wasm
  python3 -I cold.py $n $pid $p 127.0.0.1:15420 out swcache 2
done; done
python3 -I phases.py out/cold.jsonl
```

Scenarios: `restart` (kill the worker, request again), `busyN` (serve N objects first), `retire` (two apps on a
supervisor started with `"max_apps = 1"`), `swcache` (switch between two versions with the same component), `switch`
(a new version with a new component), `new` (a new app with a new component). Phase marks: `marks2.patch` in the
same directory adds `nanovm::mark` calls; apply, build, revert.

## Docs to fold in (orchestrator)

README's "The jail", "Apps" (step 3) and "How many apps" are corrected in the docs commit. PLAN.md, where it
describes a worker's start: a worker builds its engine from the node's config before it is given an app, one is kept
spawned ahead, and a worker loads its compiled component by the registry's digest without reading the component.
