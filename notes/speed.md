# speed: earlier production-path wins, ported and measured again

Branch `cf/speed`, from `cf/main-next` (`0ac2849`, then `3dafe33` with W-9 local capture). The earlier
branch `cf/optimistic-feynman-62f1a0` measured a set of wins on the jailed HTTP path and on app restarts
that never reached `main`. This note says which ones `main` already had, what was ported, and what each
does on this base.

## What main already had

| Earlier change | On main as | Same? |
|---|---|---|
| H1 `1b6b882` epoch deadline reset when a request starts | `0e33e52` (journal follow-through), `serve`'s `set_epoch_deadline(1)` | yes, same line and comment |
| H5 `e88acd5` the control runtime's own link pool | `0e33e52`: `on_thread_start(set_thread(threads))`, `Dialer::new(requests, threads + 1)` | yes |
| C3 `e082357` reuse a previous worker's empty scratch directories | `0e33e52`: `Scratch::new`, `scratch_left_by_a_previous_run` | yes, with the test |

## What was ported (one commit each, in this order)

| Port | Original | Issue | What | Runtime lines |
|---|---|---|---|---:|
| H2 | `4805ba9` | N-220 | a heavy request yields its thread at every tick, even when a lane is free | +9 |
| H3 | `4c37fad` | N-221 | the store re-polls on its own wakes, bounded by the epoch yield and a tick | +49 |
| H3 | `932be36` | N-222 | a jailed worker exports telemetry from a thread of its own | +12 |
| H4 | `fb7055d` | N-223 | link sockets registered for reads only | +64 |
| C4 | `99d0f55` | N-224, N-225 | a worker spawned ahead builds its engine before its app; the jail refuses a second thread | +54 |
| C5 | `77497df` | N-226 | a worker opens its cached component file and keys its compile by the digest | +21 |

In all +209 runtime lines (CI's count): 5,184 → 5,393 on the first base, 5,419 → 5,628 on the merged one. Everything applied with small
conflicts: the probe's `/selfwake` route went into main's `util`-based probe, the export thread's dialer
line met C4's `Node::with_engine`, and a registry test on main read `Pulled::wasm` (now
`Pulled::component`).

## Results

Measured on the shared 4-core VM while other agents built and benchmarked (load average 10–30 for the
whole session; one idle window closed during a build). So:

- **256 clients** ran at `nice -10` (supervisors, workers and oha). Wall-clock numbers stayed noisy; CPU
  per request (from `/proc/<pid>/stat`, whole process) less so.
- **One client** ran under `SCHED_FIFO` (supervisor, worker and oha set with `chrt` only for each
  measured run; one client is under one core, so this starves nobody). Without it, the one-client CPU
  numbers moved ±30% between rounds and showed nothing; with it, rounds agree within a few percent. It
  stands in for an idle machine, where a woken thread runs at once.
- Variants run side by side as separate supervisors and are measured in alternating order (reversed every
  other round). Ratios are taken per round against base, then the median and range are given.
- App restarts ran with the supervisor and client under `SCHED_FIFO` too (short bursts).

### HTTP through supervisor + jailed worker (counter, 100 objects)

One client, SCHED_FIFO, 5,000 requests per run, medians [range]:

| | base | +H2 | +H3 (both) | +H4 | tip (+C4, C5) |
|---|---:|---:|---:|---:|---:|
| CPU per request, first base, 8 rounds | 586 µs | 588 | 408 | 361 | — |
| CPU per request, merged base, 6 rounds | 467 µs | 492 | 335 | 283 | 302 |
| paired ratio to base (merged) | 1 | 1.02 [0.91–1.16] | 0.70 [0.66–0.82] | 0.62 [0.48–0.68] | 0.63 [0.58–0.70] |
| requests/s (merged) | 2,820 | 2,675 | 3,226 | 3,461 | 3,202 |
| p50 (merged) | 0.333 ms | 0.345 | 0.280 | 0.264 | 0.281 |
| voluntary context switches per request: supervisor / worker | 3.04 / 4.66 | 3.03 / 4.67 | 3.04 / 2.11 | 2.04 / 1.13 | 2.04 / 1.13 |

A third set (`http5`, first base, 6 rounds) gave base 528 → H4 304 → tip 321 µs (×0.58 [0.56–0.60]
and ×0.60 [0.58–0.65]). The earlier branch measured −42% on an idle machine; this reproduces it.
Context switches were the same in every set, with or without FIFO.

256 clients, nice −10, 100,000 requests per run; paired ratios to base pooled over three sets
(`http2`, `http5`, `m1`; 12–18 rounds):

| | CPU per request | requests/s | p50 | p99 |
|---|---:|---:|---:|---:|
| +H3 (both commits) | 0.95 [0.75–1.20] | 1.05 [0.67–1.49] | 0.98 | 0.83 [0.61–1.25] |
| +H4 | 0.935 [0.84–1.26] | 1.05 [0.77–1.45] | 0.93 | 0.82 [0.59–1.67] |
| tip | 0.94 [0.68–1.07] | 1.06 [0.77–1.58] | 0.86 | 0.94 [0.53–1.55] |

So at saturation on a loaded machine: CPU about −6%, p99 −6 to −18%, throughput within noise. The earlier
branch's +16% throughput and −40% p99 came from an idle machine; this session never had one long enough.

**Request spans delivered at 256 clients** (`http2`, 6 rounds each): base 98.1% [90.3–100], H3's waker
without its export thread **77.1% [62.1–85.6]**, with it 99.7% [96.5–100]; merged base 97.8% [91.9–100],
H3 100% [97.0–100], tip 99.3% [96.4–100]. The export thread is required with the waker.

### App restarts (first request to an app whose worker is gone)

Dev-mode supervisor, loopback registry, counter guest (392 KB; "5.6 MB" is the same guest with a 5 MiB
custom section, standing in for a large component such as a JavaScript app), 3 alternating rounds,
medians [range]. CPU is on the request's path; "spare" is the next worker's start-up, off the path.

| | base | +C4 | +C5 |
|---|---:|---:|---:|
| restart after a kill (first base, n=15) | 21.5 ms [17.9–35.1] | 11.9 [8.9–151.6] | 10.8 [8.7–11.5] |
| worker / supervisor CPU | 16.7 / 4.4 ms | 8.0 / 4.3 (+9.0 spare) | 7.4 / 3.8 (+9.2 spare) |
| restart, 5.6 MB component (n=9) | 42.3 ms [40.5–48.9] | 31.2 [28.6–206.6] | 8.9 [8.0–12.6] |
| worker / supervisor CPU | 32.8 / 11.4 ms | 21.4 / 11.6 | 6.9 / 3.8 |
| switch to a cached version (n=6) | 22.0 ms | 12.7 | 13.8 |

H4 changed none of these (19.6 / 41.9 / 22.3 ms). On the merged base, base against tip: restart 19.9 →
10.2 ms, 5.6 MB 41.1 → 11.0 ms, cached switch 22.0 → 14.1 ms, LRU retirement (`max_apps = 1`, two apps
alternating, n=18) 22.4 → 12.9 ms; worker CPU on the path 16.5 → 6.8 ms. A spare holds 2.9 MB of
anonymous memory and 12 MB of shared binary pages.

Outliers of 60–300 ms (with normal CPU, so waiting, not working) appeared in every build, base included.
`strace` put all of a start's `fsync`s in the supervisor's `app_dirs`: 30 directory syncs per start
(`sync_ancestors` of three directories, each up to `/`), median 0.19 ms, up to 16 ms each while other
agents loaded the disk (N-228).

### In-process `bench --quick`

Base, +H2 and +H3 (both commits) alternating, 9 rounds at nice −10 and load 20–30, so absolute numbers
are far below a quiet machine's (warm GET 8k/s against ~70k); medians [range]:

| | base | +H2 | +H3 |
|---|---:|---:|---:|
| warm GET, 256 clients: requests/s | 8,068 | 8,247 | 9,704 |
| p50 / p99 | 19.1 / 178 ms | 21.3 / 150 | **1.25** / 202 |
| warm GET, one client: p50 | 0.041 ms | 0.034 | 0.036 |
| cold start: p50 / p99 | 194 / 758 ms | 212 / 751 | 102 / 938 |
| POST /incr, 256 clients: requests/s | 282 | 272 | 312 |
| neighbours alone: p50 / p99 | 0.12 / 25.8 ms | 0.15 / 27.7 | 0.08 / 22.8 |
| neighbours while 16 attackers loop: p50 / p99 | 0.35 / 165 ms | 0.24 / 131 | 0.08 / 150 |
| KB per live object | 117 | 117 | 117 |

This is the trade-off the earlier branch reported: with H3 medians fall several-fold and throughput
rises, while the in-process tail at saturation rises (warm GET p99 +13%, cold-start p99 +24%). Over HTTP
the p99 falls instead. H2 alone moves the attacked neighbours' p50 and p99 down. The ranges are wide; only
the medians' direction is solid.

## Side effects found

- **CPU kills reach the client more often with H3 (N-227).** With a 2 ms CPU budget at 64 clients (so
  kills are common), 3 rounds of 50,000 requests: base turned about 70% of its kills into failed responses
  (500, or 502 `worker-down`) and the rest landed after the response was out; with H3 about 95% failed.
  After replying, the actor now keeps running its post-return work in the same poll, so a kill more
  often lands before the worker's HTTP task has written the response; the abort then fails that response
  (502 if not even its head was out). The failures never outnumbered the kills, there were no truncated bodies on
  either build, and a test of the alternative (a pooled link the worker closed) passed unchanged, so
  the 502s are the killed requests themselves, not neighbours. With the
  counter's normal 50 ms budget, kills only happen when wall time is billed under heavy machine load
  (N-640); at load 22–27 they rose from 49 to 94–134 per 600,000 requests, probably because threads that
  no longer yield voluntarily are preempted inside polls more often. Billing thread CPU time
  (`cf/cpu-clock`) should remove both; take H3 with or after it.
- **The in-process tail** rises with H3 at saturation (bench section).

## Tests

- Full suite at the port tip, serial (`env -u RUST_BACKTRACE RUST_TEST_THREADS=1 cargo test --locked -p
  nanovm`): 105 unit, 73 of 74 integration, 2 example tests. `kv_counts_against_the_disk_quota` failed
  with `cpu` (N-32/N-640); it fails the same way 3 of 3 on the unchanged base at that load.
- Merged with `3dafe33`: 111 unit, 74 integration (13 ignored), 2 example tests, all pass.
- Mutation checks on the merged tree: without H2's yield, `a_spinner_yields_its_thread_at_each_tick`
  fails 5 of 5 (the canary is sent only after the spinner's kill, done at 101–106 ms). The ported test
  raced the canary against the spinner's cold start and caught it only 2 of 8 times, so the canary now
  waits on the thread (a self-waking loop; a current-thread runtime checks its timers only every 61
  polls) until the spinner is past 10 ms. Without both of H3's bounds both fairness tests fail 3 of 3 (the
  neighbour waited 534–542 ms, until the kill). With the fixes they pass under load ~24.
- Production check without `dev` (`cargo check --locked -p nanovm --bin nanovm`): passes.
- New tests: `a_spinner_yields_its_thread_at_each_tick`, `a_self_waking_guest_yields_its_thread_at_each_tick`,
  `link::tests::bodies_larger_than_the_socket_buffer_go_through`, `workers_are_spawned_ahead`,
  `jail::tests::the_jail_needs_one_thread`, `a_cached_compile_is_used_without_reading_the_component`;
  `jail_blocks_escapes` now probes from a single-threaded forked child.

## Reproducing

Scripts are in the scratchpad's `speed/bin/`: `start.sh VARIANT...` (sink on 19318, registry on 19400,
supervisors from `target/speedrun/nanovm-<variant>` on 19480 + 10·i at nice −10; `BASEIDX` offsets the
ports, `EXTRA` adds config lines), `ab.sh`/`ab2.sh` (alternating HTTP rounds; `RT=1 ab2rt.sh` for
SCHED_FIFO), `measure.py` (one oha run: CPU, context switches, spans), `summ.py` and `paired.py`
(medians and per-round ratios), `coldab.sh` with cold2's `cold.py` (restart, 5.6 MB restart, cached
switch; `retire` with `EXTRA="max_apps = 1"`), `benchsumm.py`. Variants are
`cargo build --locked --release -p nanovm --features dev --bin nanovm` at each commit, copied by
`buildvar.sh`. The data root must be reachable by the jailed uids (`target/speedrun`, not `/tmp/claude-0`).
