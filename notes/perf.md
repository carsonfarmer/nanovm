# perf: where a warm request's CPU goes, and what changed

Branch `cf/perf`, from `491e790`. Everything here was measured on the shared
4-core VM while four other agents compiled (load average 15–28), so the
numbers are CPU time per request, A/B alternated, at `nice -10`. Wall-clock
throughput was not trustworthy. The orchestrator's idle-machine runs are the
authoritative ones.

## What changed

### 1. The journal no longer asks the kernel where `/data` is on every open (`be6ea35`)

Every file a guest opens under `/data` went through `Journal::local_at`,
which read `/proc/self/fd/N` (a `readlink`) to find out where the directory
it opened from is now, so it can tell whether the path stays inside the
volume. The counter's warm GET opens `/data/count`, so every request paid a
procfs path walk. The volume itself never moves; only its subdirectories can
be renamed while open. Now a descriptor whose device and inode are the
volume's resolves to the volume directly, and subdirectories still ask.

- Profile: `local_at` was 8.7% of in-process CPU, `readlink` alone 4.5%.
- HTTP, oha c256, 100 objects, supervisor + jailed worker, 3 alternating
  pairs: worker CPU 92.1 → 85.0 µs/request (median; 88.2–92.7 vs 81.0–86.4),
  supervisor + worker 132.1 → 122.9 µs/request (−7%).
- In-process `bench --export`: 69.3 / 67.4 → 59.0 / 62.5 µs/request
  (load generator and sink included). The other runs aborted, in both
  variants, because the loaded machine pushed some requests past the 50 ms
  CPU budget (wall time inside polls).
- Test: `journal::tests::directory_descriptors_resolve_where_they_are_now`
  (a renamed subdirectory still resolves to its new place; it fails if every
  descriptor is taken for the volume).

### 2. The supervisor stamps worker telemetry without decoding it (`97dabf5`, N-74)

The supervisor must replace each worker batch's resource (app, digest) and
drop identity attributes a worker might forge. It did this by decoding the
whole OTLP batch into structs and encoding it again. That was 24% of the
supervisor's control/export threads, and a compromised worker could make it
far worse (found by sec): 1 MiB of 2-byte empty spans decoded into 278 MB in
308 ms, on the single control thread that also serves every worker's egress.

Now `stamp` walks the protobuf bytes once: it replaces each resource
(including repeated resource fields), drops `service.name`,
`service.version`, `nanovm.app` and string-table-indexed keys from scope,
span, event, link and log attributes (the last copy of a key decides, as
decoders do), and copies everything else as bytes. The span-level scrub is
kept on purpose: the Collector's span-metrics connector reads `nanovm.app`
and `service.version` from span attributes before resource attributes, so
removing it would let a worker charge its requests to another app.

| per batch (release, loaded machine) | before | after |
|---|---|---|
| normal: 256 request spans, 119 KB | 837 µs, 802 KB allocated | 176 µs, 120 KB |
| hostile: 1 MiB of empty spans | 308 ms, 278 MB allocated | 9.9 ms, 3.1 MB |

For normal traffic that is 3.3 → 0.7 µs of supervisor CPU per request
(about 7% of the supervisor's share). No HTTP A/B was run for this change
before the container rebooted.

Tests: `stamping_a_batch_costs_about_its_size` (a counting allocator; the
old decoding stamp fails it at 277,873,410 bytes) and
`stamping_matches_decoding_and_scrubbing` (output equals decode + scrub for
repeated resources, duplicate keys, string-table keys, events, links, scopes
and logs; dropping the event scrub, letting the first key win, keeping the
worker's resource, or ignoring string-table keys each fail it). The two
jailed-worker telemetry integration tests pass.

### 3. The bench prints CPU per request (`3c0799d`)

The warm GET rows and `--export` print process CPU per request, so A/B runs
on a busy machine can compare CPU, not only wall-clock throughput.

Runtime size: 5,346 → 5,428 lines (+79 stamping, +3 journal).

## Where the CPU goes (frame-pointer profiles, before these changes)

HTTP at c256, ~115–130 µs per request in all:

- **Supervisor, ~36 µs.** The front-end thread is 78% of it, and half of
  that is the kernel: TCP to the client (on loopback the sender also pays the
  client's receive path and wakeup), the link's unix-socket reads and
  writes, and waking other threads. hyper (client and server halves) is
  about 14% of the thread, tokio 9%, telemetry about 4%. The control and
  export threads are the other 22%: encoding the supervisor's own spans
  (37% of them), stamping worker batches (24%, now mostly gone), reqwest
  (18%).
- **Worker, ~78–88 µs.** Wasmtime's component-model machinery 24%, guest
  code 20%, kernel 16%, allocator and memcpy 15%, tokio 6%, `local_at` 8%
  (now mostly gone), hyper about 4%, telemetry 2% plus its export thread.
- **Syscalls are already minimal under load:** 4 per request in the
  supervisor (TCP read, link write, link read, TCP write) and 5 in the
  worker (link read and write, plus the guest's open, stat and the readlink
  removed above); epoll waits are batched.
- **In-process** (`bench --export`) looks like the worker without the link:
  wasmtime 25%, guest 19%, kernel 11%.

At c=1 everything costs about 3.5 times more per request (~400 µs). The
supervisor makes 2.5 and the worker 4 voluntary context switches per
request (2 and 1 would do), and caches are cold after every wakeup.

### Hypotheses checked

- Per-request `TraceContextPropagator`: rejected, it is a zero-sized value.
- Span creation and attributes: 1–3% per process; not the main cost.
- Supervisor `stamp()` decoding and re-encoding: confirmed; fixed (above).
- `server::handle` URI rebuild, `AppRef::from_headers`, `app_name`: under
  5% of the supervisor front thread together (N-43).
- Link framing (hyper over socketpairs): about 5% of the total for both
  ends; a custom framing would save maybe 4%. Not changed (W-21).
- Worker runtime threads (1, 2, 4): no clear CPU win at c256 on this noisy
  box; one thread had fewer context switches at c=1 but cannot use more
  than one core. Not changed.
- Thread hops: small under load, large at low concurrency (N-40, N-41,
  N-42). A waker that re-polls the actor when it wakes itself during its own
  poll did not remove the extra switches; not committed.

## How it was measured (to reproduce)

- Builds: `cargo build --locked --release -p nanovm --features dev --bin
  nanovm --example dev-registry --example bench`, baseline from the
  starting commit, binaries copied aside under distinct names.
- In-process: `bench --export` (3 × 200k warm GETs over 100 objects at
  c256, decoded OTLP sink), alternating baseline and change.
- HTTP: `dev-registry 127.0.0.1:15400`; each variant a dev-mode
  `nanovm serve` with its own data root, `max_live_objects = 1100`,
  `memory_reservation_mb = 64`, telemetry to a span-counting OTLP sink;
  both variants running at once. Warm with 30–50k requests, then alternate
  `oha --no-tui -n 100000 -c 256 -H "nanovm-app: $APP" -H "nanovm-trust:
  dev dev" --rand-regex-url "http://127.0.0.1:$PORT/o[0-9]{2}/"`. CPU is the
  utime+stime delta over every thread of the supervisor and its worker
  (`/proc/<pid>/task/*/stat`), including a 2.5 s drain for exports, divided
  by requests. Every run returned HTTP 200 except a few CPU-budget kills
  (500) caused by machine load, in both variants.
- Profiles: `perf record -e cpu-clock --call-graph fp` on a build with
  `RUSTFLAGS="-C force-frame-pointers=yes"`; `perf record -e
  sched:sched_wakeup` for who wakes whom; `strace -c -f` for syscalls per
  request.

## For the orchestrator

- No README/PLAN/HANDOFF edits. README "What the jail costs" could mention
  that the supervisor stamps telemetry at the wire level.
- Not run here: the full serial suite and the production `cargo check`
  (container reboot and low disk). Unit tests (110) and the two jailed
  telemetry integration tests pass.
