# hop: a cheaper trip through the jail

Branch `cf/hop`, from `2ee94bc` (wave 1 merged). The goal was to make the production path
cheaper: client → supervisor → link → jailed worker → object → back.

## What was wrong

At one client a warm `GET /` cost about 3.5 times the CPU it costs under load. The work per
request is the same either way, so the extra cost had to come from waking threads. Counting
voluntary context switches per request (each one is a thread going to sleep and being woken
again) gave this:

| | ideal | measured |
|---|---|---|
| supervisor | 2 (wait for the client, wait for the worker) | 2.6–3.0 |
| worker | 1 (wait for the next request) | 4–5 |

Recording who woke whom (`perf record -e sched:sched_waking --call-graph dwarf`) and where an
object's task woke itself (a uprobe on a marker function) showed four separate causes:

1. **A stale epoch deadline.** Wasmtime checks a store's deadline at function entries and loop
   heads; nanovm sets it one tick (5 ms) ahead after each check. An object that sat idle for
   more than a tick found its deadline already passed at its next request's first
   instruction, so the request "yielded" at once. Tokio treats a task that wakes itself as a
   task that yields: it puts it at the back of the queue and wakes an idle thread to steal
   it.
2. **Wasmtime wakes the object's task three times per request**, during the task's own run:
   when the guest's call returns, when its task ends, and once inside its event loop. Each of
   these had the same effect as (1). On top of that, the thread that stole the task woke
   another one ("transition from searching").
3. **Unix sockets signal "writable" whenever the other side reads.** Tokio registers every
   socket for both reads and writes. So when the worker read a request, the supervisor's
   thread woke up for nothing, and the same happened in the other direction for each
   response. TCP doesn't do this; Unix sockets do.
4. **The readiness probe's link ended up in the wrong pool.** The probe runs on the
   supervisor's control thread, but its connection was handed to front thread 0's pool.
   Every request that later used that connection woke the control thread as well
   (N-40; it shows only while that connection is the one being reused).

## What changed (one commit each, in order)

| commit | change | runtime lines |
|---|---|---|
| `1b6b882` | The epoch deadline is reset to one tick ahead when a request starts | +1 |
| `4805ba9` | A heavy request (more than 10 ms of CPU) yields its thread at every tick, even when a heavy lane is free (N-120, a fairness gap found on the way) | +9 |
| `4c37fad` | When the object's task wakes itself while it is running, it runs again at once, with no trip through Tokio. Two kinds of wake still go back to Tokio: the epoch callback's yield, and any wake once the task has run for a tick | +49 |
| `fb7055d` | Link sockets are registered for reads only. A write that finds the socket full waits on a duplicate descriptor registered for writes | +64 |
| `e88acd5` | The control runtime gets its own pool of links | +1 |

In all: 5,652 → 5,776 runtime lines (+124).

### Why the re-run on self-wake is safe (`4c37fad`)

A guest must not be able to keep a thread longer than it could before. The rule is the same as
for a guest that only computes: it gives up its thread at every tick (5 ms).
- The epoch callback marks its yield, and that wake always goes to Tokio.
- Any wake after the task has run for a tick goes to Tokio too. This also bounds a loop of
  wakes that runs no guest code.
- `4805ba9` made heavy requests yield at every tick as well. Before it, a heavy request that
  found a free heavy lane did not yield at all between 10 ms and its CPU kill.

Two new hostile tests check this on a one-thread runtime, where no other thread can rescue a
starved request:
- `a_spinner_yields_its_thread_at_each_tick`: a short request started beside a spinning one
  must finish first, in under 50 ms. Before `4805ba9` it took 102.8 ms, ending only with the
  spinner's CPU kill.
- `a_self_waking_guest_yields_its_thread_at_each_tick`: a test-only probe route
  (`crates/nanovm/tests/post-return`) runs three guest tasks that wake themselves in a tight
  loop, so each wake goes back to the host. A neighbour object's five requests must each
  finish in under 50 ms, and the self-waker must be killed at its CPU budget.

Mutation checks:
- Re-running on every self-wake (neither bound) fails both tests. The neighbour waited
  533 ms, until the kill.
- Either bound alone passes both tests.

### Large bodies on links (`fb7055d`)

`link::tests::bodies_larger_than_the_socket_buffer_go_through` sends 4 MiB up and 8 MiB back,
far more than a socket buffer holds. If the full-socket wait is registered for reads instead
of writes, the test stalls and fails.

## Results

### Before and after the whole branch

Method: HTTP A/B, base `2ee94bc` against tip `e88acd5`. Both builds served the same guests.
- 5 alternating rounds, run right after a container restart (load average about 5: other
  agents had restarted too).
- 100,000 requests at 256 clients and 10,000 requests at one client per round.
- CPU is user + system time over every thread of the supervisor and its worker, divided by
  requests.

| | before | after |
|---|---|---|
| **one client**: requests/s | 3,165 | 3,746 (+18%) |
| p50 / p99 latency | 0.266 / 1.03 ms | 0.213 / 0.80 ms |
| CPU per request | 332 µs (supervisor 94, worker 244) | 221 µs (74, 147), **−33%** |
| context switches per request | supervisor 2.75, worker 4.75 | supervisor 2.04, worker 1.10 |
| **256 clients**: requests/s (median; range) | 20.6k (13.3–27.8k) | 23.2k (13.8–28.3k) |
| p50 / p99 latency | 10.9 / 35.7 ms | 10.0 / 28.2 ms |
| CPU per request | 117.7 µs (supervisor 33.7, worker 80.2) | 107.7 µs (31.4, 74.8), **−8.5%** |

Both processes now make the minimum number of context switches at one client. The worker's
1.10 is 1.04 on its runtime threads, plus the epoch ticker.

**Spans:**
- Every supervisor span was delivered in both builds.
- At 256 clients the worker lost 0.6% of its request spans before and 5% after. Its
  BatchSpanProcessor queue overflows at 20–28k requests/s on this machine (N-121, also seen
  in the wave-1 build).
- At a fixed 15k requests/s (`oha -q 15000`) the new build delivered every span in 3 of 3
  runs; the old one lost a few in 2 of 3.

So the loss depends on throughput, not on these changes. It is a limit of the telemetry path
to fix separately.

### Each change, measured on its own

These are 3 alternating rounds each, against the previous commit, while other agents were
compiling (load 7–13). CPU was noisy; context switches were steady.

| change | one client | 256 clients |
|---|---|---|
| epoch deadline (`1b6b882`) | CPU 504 → 404 µs, worker switches 4.59 → 3.98 | unchanged |
| re-run on self-wake (`4c37fad`) | worker switches 4.05 → 1.82 | 8.4k → 10.0k req/s, CPU 211 → 190 µs |
| read-only link registration (`fb7055d`) | switches: supervisor 2.59 → 2.02, worker 2.03 → 1.18; CPU 452 → 375 µs | 179 → 175 µs (noise) |
| control link pool (`e88acd5`) | fresh supervisor: control thread 1.90 → 0.03 switches per request | — |

### Neighbours under attack (in-process `bench --only attack`)

Builds compared: `2ee94bc` (base) against `4c37fad` (re-run on self-wake). The bench numbers
don't depend on the later link commits.

| 4 alternating rounds, load 6–7 | base | after |
|---|---|---|
| neighbours alone: requests/s | 34k / 88k / 66k / 62k | 91k / 90k / 96k / 66k |
| neighbours alone: p50 | 0.50–0.56 ms | 0.07–0.08 ms |
| neighbours alone: p99 | 14.4 / 3.0 / 5.9 / 5.8 ms | 5.4 / 6.0 / 5.0 / 12.1 ms |
| while 16 attackers loop: requests/s | 27.6k / 32.0k / 35.3k / 25.1k | 42.3k / 42.1k / 27.0k / 23.8k |
| while 16 attackers loop: p50 | 1.5–1.7 ms | 0.11–0.19 ms |
| while 16 attackers loop: p99 | 12.4 / 8.3 / 5.7 / 13.9 ms | 10.2 / 9.0 / 20.4 / 31.5 ms |

An earlier set of 3 rounds at load 9–11 also included `4805ba9` (heavy yield). It looked the
same as base:
- Neighbours alone, p99: base 13.0 / 13.0 / 9.8 ms, after 18.8 / 18.4 / 11.8 ms.
- While 16 attackers loop: base 12.7k / 12.6k / 15.8k req/s, after 14.4k / 18.1k / 37.6k.
- p99 under attack was 17–63 ms in both builds.

What is clear:
- Medians fell about 7 times.
- Throughput rose in most rounds.

What is not settled:
- The tail moved both ways from round to round. In two rounds under attack it was worse
  (20 and 31 ms against 6 and 14 ms).
- One 500 (`cpu`) for a neighbour in the last round, while the machine was busy.

My reading: before, every request's self-wakes woke idle threads. That cost CPU, but it also
spread work across threads, which helped when the OS took a core away from a busy thread.
These numbers need an idle machine before anyone relies on them (the orchestrator plans that
run).

## Not done

- **W-20, thread-per-core workers: not built.** Its main motivation, N-42's wakeups, is gone
  on the multi-threaded runtime. What is left for it is cache locality under load. It would
  also need heavy requests not to delay their thread's neighbours by a tick each.
- **N-43** (per-request SHA-256 of the app name in the supervisor) and **W-21/W-22** (link
  framing, SDK telemetry): not touched. With the wakeups gone, the next step is a fresh CPU
  profile at both concurrencies to see whether they matter.

## Reproducing

Builds:
- Each variant is `cargo build --locked --release -p nanovm --features dev --bin nanovm`
  (bin only, so dev-dependency features don't change the binary).
- Copy each binary to `target/hop-run/nanovm-<variant>`.
- The neighbours bench is `--example bench` at each commit.

Harness: `/home/user/wt/bench-run/bin` with `R=` pointing at a run directory the jailed
worker's uid can reach. A directory under `/tmp/claude-0` is not reachable: its parent is
mode 0700.

Two additions to `measure.py`:
- **Voluntary context switches per request.** Sum `voluntary_ctxt_switches` over
  `/proc/<pid>/task/*/status` before and after the oha run, for the supervisor and for its
  worker, and divide by requests. A per-thread-name breakdown shows which thread switches.
- **`OHA_Q`,** which passes `-q` to oha for fixed-rate runs.

Commands:

```
start.sh base tip                # supervisors on 21480, 21490; sink 21318; registry 21400
# warm each with oha -n 30000 -c 256; then, alternating the order every round:
measure.py base-c256-$r <sup pid> 21480 100000 256 "$APP" OUT
measure.py base-c1-$r   <sup pid> 21480 10000  1   "$APP" OUT
# ... the same for tip on 21490
bench-base     --only attack --json base-$r.json   # alternate with bench-<new>
```

Who wakes whom:
- `perf record -e sched:sched_waking --call-graph dwarf,16384 -p <worker pid>` during
  `oha -n 2000 -c 1`.
- Then `perf script -F comm,tid,event,trace,ip,sym`, grouped by the first non-tokio frames.

## For the orchestrator

- No README, PLAN or HANDOFF edits.
- README "The jail" → "Links" could say that link sockets are registered for reads only, and
  why.
- README "What the jail costs" should take its new numbers from your idle-machine run.
- Tests:
  - Full serial suite at `e88acd5`: 116 unit, 76 integration (10 ignored) and 9 JavaScript tests,
    0 failures. The production `cargo check --bin nanovm` (no `dev` feature) passes.
  - New tests: `a_spinner_yields_its_thread_at_each_tick`,
    `a_self_waking_guest_yields_its_thread_at_each_tick` and
    `link::tests::bodies_larger_than_the_socket_buffer_go_through`.
