# What changed, and the evidence

[IMPROVEMENTS.md](../IMPROVEMENTS.md) is the per-change catalogue (justification, tests, exact commits to
cherry-pick, prerequisites, risk). This page is the evidence index.

This folder records an improvement effort run as parallel workstreams, each on its own branch,
merged into `cf/optimistic-feynman-62f1a0`. Every claim below links to a workstream note with the
method, the raw numbers and the tests that prove it. Numbers are from one 4-core Linux VM; read them
as before/after on the same machine, not as absolute capacity.

## Wave 1 at a glance

| Area | Before | After | How we know |
|---|---|---|---|
| Warm GET, in-process, 100 objects, 256 concurrent | 40.9k req/s | 47.9k req/s (+17%) | idle-machine A/B, median of 3 alternating runs |
| Cold start (first request to a new object) | 3.26k/s | 3.72k/s (+14%) | same |
| Churn (new object + eviction) | 2.77k/s, p99 67 ms | 4.74k/s (+71%), p99 41 ms | same |
| First request after a crash (fresh instance) | 703/s, p99 1.24 ms | 1.07k/s (+52%), p99 0.66 ms | same |
| Memory per live object | 117 KB | 90 KB (−23%) | same |
| Neighbours' throughput while 16 attackers loop | 29.0k req/s | 34.1k req/s (+18%) | same |
| HTTP through supervisor + jailed worker | 17.5k req/s, 178 µs CPU/request | 17.4k req/s, 176 µs (unchanged) | idle-machine A/B, oha c256 |
| Security bugs found and fixed | — | 5 (each with a test that failed before) | [sec.md](sec.md) |
| Crates in a normal build | 322 | 265 (−18%) | [lean.md](lean.md) |
| Release binary | 54.5 MB | 49.1 MB (−10%) | same |
| Languages for apps | Rust (raw bindings) | Rust SDK, JavaScript | [dx.md](dx.md), [js.md](js.md) |
| Run an app locally | registry + dev mode + headers | `nanovm run app.wasm` | [dx.md](dx.md) |
| Tests | 107 unit, 67 integration | 115 unit, 74 integration, 9 JavaScript | full suite on an idle machine |

The in-process rows compare the whole product before and after (runtime and guests); the HTTP row
compares only the runtime (both builds served the same guests). `POST /incr` rows are left out of the
table because the counter guest itself changed (it now writes in place, since commits already make a
request's writes all-or-nothing; see [dx.md](dx.md)).

**The production HTTP path did not get faster in wave 1** (the process hop dominates it). Wave 2 fixed
that; see below.

## Wave 2 at a glance (final branch against `491e790`, same idle VM)

| Area | Before | After | How we know |
|---|---|---|---|
| HTTP through supervisor + jailed worker, 256 clients | 23.5k req/s, p99 34.7 ms, 130 µs CPU/request | 27.2k req/s (+16%), p99 21.0 ms (−40%), 110 µs (−16%) | oha, 6 alternating rounds |
| HTTP, one client | 2.8k req/s, p50 0.31 ms, 381 µs CPU/request | 3.8k req/s (+36%), p50 0.22 ms (−31%), 220 µs (−42%) | oha, 3 rounds |
| Request spans delivered at 256 clients | 99.9% | 100% | counting OTLP sink |
| In-process warm GET, one client | 8.6k req/s | 16.2k req/s (+87%) | `bench --quick`, 4 rounds |
| In-process warm GET, 256 clients: throughput / p99 | 65.9k / 13.2 ms | 75.0k (+14%) / 22.0 ms (worse) | same |
| App restart (first request after its worker is gone) | 20.8 ms; JS app 73.5 ms | 7.0 ms; JS app 9.7 ms | [cold2.md](cold2.md) (loaded) |
| JavaScript warm GET | Boa 438 µs CPU, 5.4 MB component | QuickJS 218 µs, 1.5 MB | [qjs.md](qjs.md) (loaded) |
| Read-only first request using key-value | 32 ms p50 | 1.4 ms | [lean2.md](lean2.md) |

The only regression is the in-process p99 under saturation (the store waker, H3 in
[IMPROVEMENTS.md](../IMPROVEMENTS.md)); its medians and the HTTP p99 improve.

## Workstreams

- [perf.md](perf.md): profiles of the hot path; the journal no longer asks procfs where `/data` is on
  every open; the supervisor stamps worker telemetry at the wire level (a hostile 1 MiB batch went from
  278 MB / 308 ms to 3 MB / 10 ms).
- [cold.md](cold.md): empty `/tmp` directories are reused; every live object holds 23% less memory.
- [sec.md](sec.md): N-70 datagram sockets out of the jail, N-71 forged WebSocket events, N-72 publish
  parsing amplification, N-73 domain fronting past the egress allowlist, N-75 host memory held by guest
  headers; plus everything checked and found sound.
- [lean.md](lean.md): dependency features trimmed, oci-spec replaced by a small tested parser,
  journal and kv simplified.
- [dx.md](dx.md): the `nanovm-sdk` crate, `nanovm run`, the live chat example.
- [js.md](js.md): JavaScript apps with a Workers-style `fetch(request, env)` API, pre-initialized so a
  cold start skips engine start-up.
- [hop.md](hop.md): the jailed HTTP path: epoch deadline, heavy-lane yields, the store waker, read-only
  link registration, the control runtime's link pool.
- [cold2.md](cold2.md): app cold start: scratch reuse across workers, spawn-ahead workers, cached
  component files.
- [lean2.md](lean2.md): kv buckets made on first write, a journal commit fix, 14 simplifications.
- [qjs.md](qjs.md): JavaScript on QuickJS instead of Boa.

## Verification of the merged branch

- Full locked suite, serial, idle machine: 120 unit, 78 integration (10 ignored), 9 JavaScript; 0 failures.
- SQLite durability suite (`--ignored`): 8 of 8, including the crash-cut matrices and a jailed worker
  killed mid-commit; the composed SQLite example passes.
- Production build without the `dev` feature checks.

## Reproducing the benchmarks

In-process: build `bench` at both commits and run `bench --quick --json out.json` alternately
(3 rounds each). HTTP: a dev-mode supervisor per build, the loopback dev registry, a counting OTLP
sink, and `oha -n 200000 -c 256` over 100 objects, alternating builds; process CPU comes from
`/proc/<pid>/task/*/stat`. The scripts used are described in [perf.md](perf.md).
