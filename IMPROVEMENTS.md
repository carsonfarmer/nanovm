# Improvements on `cf/optimistic-feynman-62f1a0`: what each change does, why, and how to take it

This branch is a catalogue of independent improvements to nanovm, meant to be pulled into `main` one
targeted change at a time. Each entry below says what was wrong, what changed, the evidence, the tests
that prove it, the exact commits to cherry-pick, what it depends on, and its risk.

- **Base.** Every commit was made on top of `491e790`, which is exactly `origin/main`. The commit
  hashes below are reachable from this branch, so `git cherry-pick -x <hash>` works directly.
- **"Applies cleanly"** means the listed commits were cherry-picked in order onto `origin/main` in a
  scratch worktree without conflicts. **"Verified"** means the result was also built and tested; the
  whole merged branch was verified (below), and individual picks are verified where noted.
- **Runtime lines** use CI's count (non-blank, non-comment lines of `crates/nanovm/src` before each
  file's test module). The owner tracks it; every entry states its delta.
- **Numbers.** "Idle A/B" means the old and new builds ran alternately on an otherwise idle 4-core VM
  (3 rounds, medians). Numbers taken while other agents were compiling are labelled "loaded"; they
  compare CPU time per request, which noise affects far less than throughput.
- The per-workstream notes in [notes/](notes/README.md) have full methods and raw numbers.

## The result in one table

Same idle 4-core VM, old and new builds alternating; "baseline" is `491e790` (= `origin/main`).

| What | Baseline | This branch | How measured |
|---|---:|---:|---|
| HTTP through supervisor + jailed worker, 256 clients | 23.5k req/s, p99 34.7 ms, 130 µs CPU | **27.2k req/s (+16%), p99 21.0 ms (−40%), 110 µs (−16%)** | oha, 100 objects, 6 alternating rounds |
| HTTP through supervisor + jailed worker, one client | 2.8k req/s, p50 0.31 ms, 381 µs CPU | **3.8k req/s (+36%), p50 0.22 ms (−31%), 220 µs (−42%)** | oha, 3 rounds |
| Request spans delivered at 256 clients | 99.9% | **100%** | counting OTLP sink |
| In-process warm GET, one client | 8.6k req/s | **16.2k req/s (+87%)** | `bench --quick`, 4 rounds |
| In-process warm GET, 256 clients | 65.9k req/s, p99 13.2 ms | 75.0k req/s (+14%), **p99 22.0 ms (worse)** | same |
| Cold start (first request to a new object) | 3.31k/s | 3.78k/s (+14%) | same |
| Churn (new object + eviction) | 2.59k/s | 5.47k/s (+111%) | same |
| First request after a crash | 726/s | 1,172/s (+61%) | same |
| Memory per live object | 117 KB | 90 KB (−23%) | same |
| App restart (first request after its worker is gone) | 20.8 ms; JS app 73.5 ms | 7.0 ms; JS app 9.7 ms | dev supervisor, alternating builds (loaded) |
| First request after a busy worker restarts (1,000 objects) | 1,211 ms | 51 ms | same |
| JavaScript warm GET (CPU per request) | — (no JS before) | 218 µs on QuickJS (Boa: 438 µs) | js-bench (loaded) |
| Security bugs fixed, each with a test that failed before | — | 5 (+ 2 found in wave 2) | see S1–S6, N-120, N-130 |
| Distinct crates in a normal build | 302 | 251 (−17%) | `cargo tree -e normal` |
| Release binary | 54.5 MB | 49.4 MB (−9%) | `cargo build --release` |
| Runtime lines (CI count) | 5,346 | 5,687 (+6%) | buys `nanovm run`, wire stamping, spawn-ahead, the waker, read-only links |

The one regression: under saturation in-process, p99 rises (warm GET at 256 clients 13 → 22 ms;
neighbours 2.6 → 4.0 ms) because of the store waker (H3), while medians fall sharply (neighbours' p50
0.49 → 0.10 ms). On the production HTTP path the same change lowers p99 (it is part of the −40%).
H3 is separable if you'd rather keep the old tail.

## Verification of the whole branch

| Check | Result |
|---|---|
| Full locked suite, serial, idle machine (`env -u RUST_BACKTRACE RUST_TEST_THREADS=1 cargo test --locked -p nanovm`) | 120 unit, 78 integration (10 ignored), 9 JavaScript: all pass |
| SQLite durability suite (`--ignored sqlite_`), including crash-cut matrices and a jailed worker killed mid-commit | 8 / 8 pass; composed SQLite example passes |
| Production build without the `dev` feature (`cargo check --locked -p nanovm --bin nanovm`) | passes |
| Baseline at `491e790` for comparison | 107 unit, 67 integration (10 ignored) |

## Summary

| ID | Change | Benefit | Runtime lines | Take on its own? |
|---|---|---|---:|---|
| **Security** | | | | |
| S1 | seccomp: stream socketpairs only (N-70) | jail escapee can't reach host datagram sockets (journald, syslog, notify) | +1 | yes |
| S2 | refuse forged WebSocket events however spelled (N-71) | clients can't forge a `Connection-Id` | +1 | yes |
| S3 | parse GRIP publishes into typed structs (N-72) | 8 MiB publish: 192 MiB → <1 MiB in the shared proxy | +16 | yes (see S6) |
| S4 | egress sets `Host` from the checked authority (N-73) | no domain fronting past the allowlist | +2 | yes |
| S5 | bound host-held header maps by `memory_mb` (N-75) | guest can't hold ~1.25 GiB of headers against 64 MiB | +7 | yes, after its ISSUES entry |
| S6 | stamp worker telemetry at the wire level (N-74) | hostile batch 278 MB / 308 ms → 3 MB / 10 ms; normal 4.7× less CPU | +77 | yes (see S3) |
| **Speed and density** | | | | |
| P1 | journal: no `readlink` for the volume's own descriptor | −7% CPU per HTTP request (loaded); `readlink` was 4.5% of in-process CPU | +5 | yes |
| C1 | reuse empty `/tmp` directories; canonicalize once | churn 2.4×, p99 133 → 58 ms (loaded); +71% idle A/B | +31 | yes |
| C2 | shrink every live object | RSS per object 116 → 89 KB (−23%) | +5 | yes |
| **Smaller and simpler** | | | | |
| L1 | build only the dependency features used | 322 → 286 crates, two C builds gone (with L2: release binary −10%) | 0 | yes |
| L2 | parse OCI references ourselves; drop oci-spec | −21 crates (regex, 6 proc-macros), per-thread parse cache gone | +40 | yes |
| L3 | journal: find unsaved bytes as gaps | simpler, same records | −9 | yes |
| L4 | kv: bindgen splits errors from traps | simpler | −8 | yes |
| L5 | drop needless generality (config, metrics, proxy, ws) | simpler | −18 | yes |
| **Developer experience** | | | | |
| D1 | `nanovm-sdk` Rust guest SDK; guests ported | counter 151 → 87 lines, hostile 359 → 249 | 0 | yes (republishes test guests) |
| D2 | in-process nodes deliver WebSocket publishes (N-60, N-61) | publishes work in tests, bench and `run`; bad paths 404 at once | +13 | yes |
| D3 | `nanovm run app.wasm` | one command to run an app locally, with sockets and reload | +103 | after D2 |
| D4 | `examples/chat` live multi-room chat | showcase, tested end to end | 0 | after D1, D3 |
| J1 | JavaScript apps (`fetch(request, env)`) | apps in JavaScript; zero runtime lines | 0 | yes |
| J2 | JavaScript on QuickJS instead of Boa | warm CPU −50%, memory −27%, component 5.4 → 1.5 MB | 0 | after J1, D1 |
| **Wave 2: the jailed HTTP path** | | | | |
| H1 | reset the epoch deadline when a request starts | warm requests stop moving threads; one client −20% CPU (loaded) | +1 | yes |
| H2 | heavy requests yield at every tick (N-120) | a short request beside a spinner: 103 ms → <50 ms | +9 | yes |
| H3 | re-poll the store on self-wakes, bounded at a tick (N-42) + export thread (N-121) | worker context switches 4.05 → 1.82 per request; most of the HTTP gain | +66 | after H2; take both commits |
| H4 | register link sockets for reads only (N-41) | one fewer cross-process wakeup per request; adds two `unsafe` blocks | +64 | after S5 |
| H5 | the control runtime's own link pool (N-40) | control thread 1.9 → 0.03 wakeups per request | +1 | yes |
| **Wave 2: app cold start** | | | | |
| C3 | reuse a previous worker's empty scratch directories (N-150) | first request after a busy worker restarts: 1,211 → 51 ms | +10 | after C1 |
| C4 | spawn the next worker ahead; jail refuses >1 thread (N-151, N-152) | app restart 21 → 10 ms | +54 | branch order |
| C5 | open the cached component file instead of piping and hashing it (N-153) | JS app restart 78 → 16 ms | +21 | after C4 |
| **Wave 2: smaller, and fixes found on the way** | | | | |
| K1 | kv buckets made by their first write (N-110) | read-only first request: 32 → 1.4 ms p50 (JS) | +3 | after D1, J1 (tests) |
| K2 | journal: a path under what became a file no longer fails the commit (N-130) | a guest can't make its own commits fail | 0 | yes |
| K3 | 14 simplifications (helpers, prost varints, `tokio::fs`, `Default`, …) | −102 runtime lines | −102 | see list |
| K4 | test-only code moved below CI's count | the count stops including 87 lines of tests | −87 | see list |
| K5 | deterministic tests (N-91, N-132) | no load flakes | 0 | yes / after C1 |
| **Maintenance** | | | | |
| M1 | wasmtime and WASI crates 50.0.1 | five security advisories fixed upstream | 0 | yes (regenerate the lockfile) |
| M2 | bench prints CPU per request, fds/maps/heap per object | A/B on a busy machine | 0 | yes |
| M3 | test fixes: free publish port (N-92) | no failures when port 8444 is taken | 0 | yes |
| M4 | docs: README front door, notes, PLAN/HANDOFF | clearer | 0 | with the changes they describe |


---

## Security

All six came from a fresh adversarial review of every trust boundary ([notes/sec.md](notes/sec.md),
which also lists what was checked and found sound). Each fix has a hostile test that was run against
the old code and failed.

### S1. A jailed worker can no longer send datagrams to host sockets (N-70)

- **Problem.** The worker's seccomp filter allowed `socketpair(AF_UNIX, …)` of any type, for tokio's
  signal driver. A *datagram* socket's `sendto` names its own destination, even on a pair, so code that
  escaped Wasm could send datagrams — and file descriptors via `SCM_RIGHTS` — to any named or abstract
  unix datagram socket on the host: journald, syslog, systemd's notify socket. Landlock doesn't cover
  socket sends.
- **Change.** `jail.rs`: `socketpair` is allowed only with `SOCK_STREAM` (one extra seccomp condition).
- **Evidence.** `jail_probe` (run inside a real jail by `jail_blocks_escapes`) sent a datagram to an
  abstract socket the test bound: `unix_datagram=ok` before, refused after, and nothing arrives.
- **Pick.** `32de17e` — applies cleanly. It accidentally adds an empty `.seeded` file: delete it
  (`git rm .seeded`), or pick S2 after it, whose first commit removes it.
- **Risk.** Low; tokio's signal driver uses a stream pair. Runtime +1.

### S2. Clients can't forge WebSocket events (N-71)

- **Problem.** Only the proxy may send `application/websocket-events` requests, so an object can trust
  their `Connection-Id`. The proxy refused a client request as forged only if its *first* `Content-Type`
  was exactly that type. A second header, other case, parameters, or a comma-joined value got through,
  and guests read headers in all those ways (the counter answered a forged `TEXT id` with the client's
  chosen id).
- **Change.** `ws::is_event` matches any `Content-Type` value, or comma-joined part, whose media type is
  the events type, ignoring case and parameters.
- **Evidence and tests.** `the_proxy_routes_hosts_by_dns` sends the forgeries through the proxy: 200 with
  `m:7` before, 400 after; a unit test covers each spelling.
- **Pick.** `fdf0fee bbc92f6` — applies cleanly. Runtime +1.

### S3. A guest's publish can't make the shared proxy allocate 24× its size (N-72)

- **Problem.** GRIP publishes (up to `max_response_body_kb`, 8 MiB by default) were parsed as
  `serde_json::Value` in the worker and again in the proxy every app shares. 8 MiB of tiny values peaked
  at 192 MiB and 200–370 ms of a proxy thread, on every request of any app.
- **Change.** `ws.rs` parses publishes into typed structs that read only the fields sockets use; the rest
  is skipped without being built.
- **Evidence.** Same 8 MiB body: <1 MiB peak, 21 ms; the worst case (8 MiB of minimal items) 42 MiB (5×).
  `publishes_parse_in_bounded_memory` counts the parsing thread's allocations and fails with the old
  parser (1 MiB held 25 MB).
- **Pick.** `fd7dfe4` — applies cleanly. **If you also take S6**, both add a test-only
  `#[global_allocator]` and the unit-test binary won't link: also apply the shared counting allocator
  from `13db20e` (its `lib.rs`, `ws.rs` and `telemetry.rs` hunks) and `181b391`'s move of it below the
  line CI counts (wave 2). Runtime +16.

### S4. No domain fronting past the egress allowlist (N-73)

- **Problem.** The supervisor checked a worker's outbound request against the allowlist by its URI host,
  then forwarded the worker's own headers, `Host` included. Guests can't set `Host` (wasi-http forbids
  it), but a compromised worker can: a request to an allowed CDN carrying another site's `Host` is
  routed to that site, so the allowlist named addresses, not hosts.
- **Change.** `egress.rs::send` sets `Host` from the checked authority.
- **Evidence.** `the_upstream_hears_the_allowed_host`: the upstream heard `elsewhere.example` before.
- **Pick.** `9890e6d` — applies cleanly. Runtime +2.

### S5. A guest can't hold many times its memory limit in host-side headers (N-75)

- **Problem.** Each `wasi:http` fields resource held up to 128 KiB in worker memory, outside
  `memory_mb`, and an instance may keep `max_resources` (10,000) of them: ~1.25 GiB against 64 MiB.
  Copies were worse: cloning a received map shares it, and the first change copies it whole before any
  size check (~4 GiB from a ~400 KB request head).
- **Change.** A request or response head is at most 64 KiB at every hop (`server::MAX_HEAD`, hyper's
  `max_header_size` on the node, links and egress; over it a client gets 431); a guest's own header map
  is at most 64 KiB; an instance holds at most `memory_mb × 16` resources (256 at 16 MiB, 1,024 at 64 MiB).
- **Evidence.** `guest_header_maps_are_bounded_by_the_memory_limit` (a route on the test-only probe
  component): with `memory_mb = 32` it held 600 maps of 60 KiB before; every part of the fix was
  mutation-checked. The counter's whole suite passes under its new 256-resource cap; so do the SQLite and
  JavaScript suites.
- **Pick.** `1ee35ae 98c1698 c5644bd` (ISSUES entries, the fix, its docs) — applies cleanly.
- **Risk.** A behaviour change for apps: requests with heads over 64 KiB get 431, and an instance's
  resource cap now scales with its memory. Runtime +7.

### S6. Worker telemetry is stamped at the wire level (N-74)

- **Problem.** The supervisor must replace each worker batch's resource (app, digest) and drop identity
  attributes a worker might forge. It decoded the whole OTLP batch and encoded it again: 24% of the
  supervisor's control/export threads normally, and a compromised worker's 1 MiB batch of 2-byte empty
  spans decoded into 278 MB in 308 ms on the single thread that also serves every worker's egress.
- **Change.** `telemetry::stamp` walks the protobuf bytes once: it replaces each resource (repeated ones
  included), drops identity keys from scope, span, event, link and log attributes (the last copy of a key
  decides, as decoders do), and copies everything else as bytes. The span-level scrub stays on purpose:
  the Collector's span-metrics connector reads `nanovm.app` and `service.version` from span attributes
  first, so without it a worker could charge its requests to another app.
- **Evidence.** Per batch, release build: normal 256 spans 837 µs / 802 KB → 176 µs / 120 KB; hostile
  1 MiB 308 ms / 278 MB → 9.9 ms / 3.1 MB. `stamping_a_batch_costs_about_its_size` fails at 277,873,410
  bytes with the old code; `stamping_matches_decoding_and_scrubbing` checks equivalence with
  decode-then-scrub and catches five mutations.
- **Pick.** `97dabf5` — applies cleanly. See S3 if you take both. Runtime +77 (wave 2's `fd1c6ea` reads
  varints with prost and removes some of it).

## Speed and density

### P1. The journal doesn't ask procfs where `/data` is on every open

- **Problem.** Every guest open under `/data` read `/proc/self/fd/N` to check the path stays inside the
  volume. The counter's warm GET opens a file per request: `readlink` was 4.5% of in-process CPU.
- **Change.** `journal.rs`: a descriptor whose device and inode are the volume's resolves to the volume
  directly (the volume can't move); subdirectories, which can be renamed while open, still ask.
- **Evidence (loaded).** HTTP through supervisor and jailed worker, 3 alternating pairs: worker CPU
  92.1 → 85.0 µs/request, total 132.1 → 122.9 (−7%). `directory_descriptors_resolve_where_they_are_now`
  fails if every descriptor is taken for the volume.
- **Pick.** `be6ea35` — applies cleanly. Runtime +5.

### C1. Empty scratch directories are reused; the volume root is resolved once

- **Problem.** Every instance made its own `/tmp` directory and removed it when it went away, and
  canonicalized its volume path (one `readlink` per component). On this VM an `rmdir` costs ~70 µs of
  CPU and, on a disk mounted with `discard`, ~3.3 ms blocking a tokio worker.
- **Change.** An app keeps the scratch directories its instances left *empty* and hands them to the next
  instance; one with anything in it is removed as before, so every instance still starts with an empty
  `/tmp`. The objects directory is canonicalized once at load.
- **Evidence.** Loaded, 4 alternating pairs: churn 1,388 → 3,359/s, p99 133 → 58 ms; first request after
  a trap p50 0.8 → 0.3 ms; syscalls per churn request 38.6 → 23.6. Idle A/B of the merged branch: churn
  +71%, first request after a trap +52%. `only_empty_scratch_is_reused` fails without the reuse and
  without the emptiness check.
- **Pick.** `f1e89d6` — applies cleanly. Runtime +31.

### C2. Every live object holds 23% less memory

- **Problem.** A heap profile of 2,000 idle objects put 36 of the 68 KB each allocates in two of our
  structures: the mailbox (tokio allocates 32 slots up front; a `Job` was 592 bytes) and the actor task
  (16.9 KB, because the store's 7.6 KB event-loop future was held twice and a rarely used publish path
  sat inline).
- **Change.** Jobs are boxed; the caller pins the event-loop future; the publish loop is boxed only
  when there's something to send.
- **Evidence.** RSS per live object 116 → 89 KB in every run; heap 67.6 → 35.7 KB; cold-start CPU and
  churn unchanged. `an_actor_is_small` guards the task size.
- **Pick.** `d0e85be` (with or without C1) — applies cleanly. Runtime +5.

## Smaller and simpler

### L1. Build only the dependency features the node uses

- **Problem.** wasmtime was built with its default features: a second compile cache, profilers, core
  dumps, the text format, guest debugging, compile-time builtins and runtime WIT parsing, none used.
  wasmtime-wasi built WASIp1, wasmtime-wasi-http built `wasi:http@0.2`, sigstore-trust-root built its
  TUF client.
- **Change.** `crates/nanovm/Cargo.toml` lists the features used. Every feature that changes what a guest
  may do or what an error says stays (gc, threads, stack-switching, backtraces, pooling, parallel
  compilation). The `.cwasm` compatibility hash is unchanged (`9a8732b36debb5f5` before and after).
- **Evidence.** Distinct crates 322 → 286; zstd-sys and ittapi-sys (C builds) gone.
- **Pick.** `868e006` — applies cleanly. **Behaviour change:** a component in WAT text form is refused
  as a bad binary (it was never a supported format). Runtime 0.

### L2. OCI references are parsed by a small parser of our own

- **Problem.** oci-spec's `Reference` is one large regex; parsing it per request was costly enough that a
  per-thread cache of parsed references had been added. oci-spec also brought regex and six proc-macro
  crates into every build.
- **Change.** `registry::Reference` parses distribution's grammar with string scans; the cache is gone.
- **Evidence.** A differential test compares it with oci-spec on oci-spec's own cases, edge cases and
  100,000 random strings (10,000+ valid): same accepted set and fields; four broken parsers each fail it.
  Crates 286 → 265.
- **Pick.** `d485694` — applies cleanly. **Behaviour change:** tags must be ASCII, as the OCI spec says
  (N-90); malformed references all report "invalid reference format". Runtime +40.

### L3–L5. Simplifications with unchanged behaviour

- **L3** `eac8d42` (journal: unsaved bytes as gaps between saved ranges; the same records in the same
  order; new `saved_ranges_merge`, three mutants caught). Runtime −9.
- **L4** `cee06aa` (kv: wasmtime's bindgen splits key-value errors from traps instead of a hand-written
  adapter). Runtime −8.
- **L5** `29c8ddd` (config, metrics, proxy and ws without generality nothing used). Runtime −18.
- Each applies cleanly on its own.

## Developer experience

### D1. `nanovm-sdk`, a Rust guest SDK

- **Problem.** A guest had to hand-write raw wasip3 bindings (the test guests shared a helper crate of
  test-helper quality).
- **Change.** `guests/sdk`: `serve!(handle)` exports an `async fn(Request) -> Result`; an `Err` (or `?`)
  rolls the request back, matching nanovm's commit rule. It covers requests and responses (optional
  serde JSON), `/data` and `/tmp` files, key-value, config, outbound fetch, calls between objects,
  WebSocket events and GRIP publishes. Every guest crate moved onto it.
- **Evidence.** counter 151 → 87 code lines, hostile 359 → 249, HTTP behaviour unchanged (the
  integration suite is the spec). No benchmark regression.
- **Pick.** `659bdbb` (and the docs in `1fcd59d`) — applies cleanly. **Note:** changing `guests/**` on
  `main` triggers `publish-guests.yml`, which republishes the test guests. The counter now writes
  `/data/count` in place (commits already make a request's writes all-or-nothing), so its `POST /incr`
  is cheaper — keep that in mind when comparing benchmarks across this change. Runtime 0.

### D2. In-process nodes deliver WebSocket publishes (N-60, N-61)

- **Problem.** A node built in-process (tests, the benchmark, embedders) sent a guest's
  `http://nanovm/publish/` to its egress policy, which denied it; only jailed workers could publish. A
  publish to any other `http://nanovm/` path was accepted by the worker and silently dropped.
- **Change.** Every node holds publishes until its request commits; a jailed worker forwards them to the
  supervisor as before, an in-process node hands them to its own `ws::Sockets`. Other paths get 404 at
  once. The proxy's upgrade tail moved into `Sockets::upgrade` so a local front end can share it.
- **Evidence.** `only_well_formed_publishes_are_held` fails without either change.
- **Pick.** `1392e99` — applies cleanly. Runtime +13.

### D3. `nanovm run app.wasm`

- **Problem.** Running an app meant a registry, dev mode, signatures off and magic headers.
- **Change.** `nanovm run <app.wasm> [--manifest nanovm.json] [--listen 127.0.0.1:8080] [--data DIR]`
  serves a component from a file with an in-process node and the node's own request path, plus what the
  proxy adds (WebSockets, refusing client-sent socket events), a loopback OTLP sink that prints a line
  per request and the app's output, and a re-exec when the file changes. It refuses non-loopback
  addresses, needs no `dev` feature, and gives egress no private-address exemption. Production `serve`
  is unchanged: signed registry apps only.
- **Evidence.** Four `run::` tests (loopback refusal, requests and console lines, sockets with
  commit-gated publishes and the forged-event refusal, restart on change), each check mutation-tested.
- **Pick.** `1392e99 309eb6f` — applies cleanly. Runtime +103 (in `main.rs`).

### D4. `examples/chat`

- A live multi-room chat: each room is an object with its history in its volume and its members as
  WebSockets; a `lobby` object indexes rooms through calls between objects; the component serves its own
  page. `run::the_chat_example_is_live_and_durable` drives two members, live updates, eviction under open
  sockets, a process restart and bad input.
- **Pick.** `659bdbb 1392e99 309eb6f d73434b` — applies cleanly. Runtime 0.

### J1. JavaScript apps

- **Problem.** Only Rust guests ran; componentize-js has no WASIp3 HTTP yet.
- **Change.** `js/` (its own workspace, zero runtime lines, no change to the import allowlist) is a guest
  component that embeds a JavaScript engine and runs an app's ES module with a Workers-style
  `export default { fetch(request, env, ctx) }`: `Request`/`Response`/`Headers`/`URL`, `env.kv` on
  `wasi:keyvalue` (committed with the response), outbound `fetch` under the egress policy,
  `env.objects.get(id).fetch()`, `env.config`, `ctx.waitUntil`, WebSocket handlers. `js/build.sh`
  compiles the app in and pre-initializes it with Wizer, so a cold start skips engine start-up; every
  host import traps during pre-initialization, so no handle, clock or randomness is baked in.
- **Evidence (wave 1, Boa engine).** Cold start 9.4 → 2.4 ms CPU with pre-initialization. Nine tests on a
  real node: routing and storage across eviction, rollback on throw, CPU and memory kills of runaway JS,
  egress denial, calls between objects, Web APIs, timers, WebSockets, nothing shared across instances
  from the snapshot (mutation-checked).
- **Pick.** `a26bfa7 c63b06f 9cde8c2 95d3212 70f9516 050885b` — applies cleanly. J2 replaces the
  engine; take its commits on top.

### J2. JavaScript runs on QuickJS instead of Boa

- **Problem.** With Boa, a warm JavaScript request cost ~440 µs of CPU (Rust: ~40–130 µs), the
  component was 5.4 MB and building an app took ~45 s; the runtime also duplicated the SDK's bindings.
- **Change.** `js/` embeds QuickJS-NG through rquickjs, built with the WASI SDK 34 (`WASI_SDK_PATH`;
  CI already pins it for SQLite). Same app API; the runtime is now an SDK app (no bindings of its own).
  Snapshot guarantees hold: the clock QuickJS reads at context start answers 0 only during that call,
  `Math.random` is seeded from the host at first use, and an out-of-memory allocation traps so the
  memory limit still kills the instance (both mutation-checked).
- **Evidence (loaded, 6 alternating rounds; instructions by valgrind).** Warm GET 438 → 218 µs CPU,
  1,566k → 428k instructions; private memory per object 352 → 257 KB; component 5.4 → 1.5 MB; each app
  builds in ~10 s instead of ~45 s. All nine JavaScript tests pass, extended for URL, TextDecoder,
  structuredClone, timers and runaway recursion.
- **Pick.** `8575937 c8cd295 fef7cb4` (SDK `Request::url()`, js-bench memory column, the switch), after
  D1 and J1 — `659bdbb a26bfa7 c63b06f 9cde8c2 95d3212 70f9516 050885b 8575937 c8cd295 fef7cb4`
  applies cleanly. Runtime 0. **Limits:** runaway recursion is a trap, not a catchable `RangeError`
  (N-140); no `performance` yet (W-65).

## Wave 2: the jailed HTTP path

Wave 1 left the production path unchanged. Profiling showed why: at one client a request crossed
threads several times in each process for nothing. Together H1–H5 and the N-121 fix give the HTTP
rows of the table at the top. Notes: [notes/hop.md](notes/hop.md).

### H1. Reset the epoch deadline when a request starts

- **Problem.** An object's epoch deadline was one tick after its last callback, so an object idle for
  more than 5 ms hit an expired deadline at its next request's first instruction and yielded; tokio
  treats a task that wakes itself as a yield, queues it at the back and wakes another thread to steal
  it. Nearly every warm request moved threads.
- **Change.** `object.rs`: the deadline is one tick from the start of each request. Budgets, heavy lanes
  and per-tick yields are unchanged.
- **Evidence (loaded).** One client: 504 → 404 µs CPU per request, p50 0.449 → 0.388 ms, worker context
  switches 4.59 → 3.98 per request.
- **Pick.** `1b6b882` — applies cleanly. Runtime +1.

### H2. Heavy requests yield their thread at every tick (N-120)

- **Problem.** Past 10 ms of CPU a request must hold a heavy lane. When a lane was free it was taken
  without returning to the executor, so a heavy request kept its thread from 10 ms until its CPU budget
  killed it; tasks behind it in tokio's unstealable LIFO slot (or on a one-thread runtime) waited.
- **Change.** The heavy path yields once before taking a lane.
- **Evidence.** `a_spinner_yields_its_thread_at_each_tick`: a short request beside a spinner on a
  one-thread runtime finished at 102.8 ms before (with the spinner's kill), under 50 ms after.
- **Pick.** `4805ba9` — applies cleanly. Runtime +9.

### H3. The store re-polls on its own wakes, within a tick; the worker exports from its own thread (N-42, N-121)

- **Problem.** Wasmtime wakes the store's task during its own poll about three times per request;
  tokio takes each for a yield and wakes idle threads to steal it, so a request woke two or three threads
  for nothing and usually finished on another one.
- **Change.** `object.rs`: the store gets its own waker; a wake during the poll polls again at once,
  while a wake from outside goes to tokio. Two bounds keep it fair: the epoch callback's yield always
  reaches tokio, and so does any wake once a poll has run for a tick (5 ms), which also bounds a self-wake
  loop that runs no guest code. Because a task can now hold its thread for up to a tick, the worker's
  telemetry export, which shared those threads, could queue behind guest work and drop spans under
  saturation; `932be36` gives it a current-thread runtime of its own with its own link pool (the
  worker-side twin of the supervisor's N-36 fix).
- **Evidence.** Worker context switches per request at one client 4.05 → 1.82 (perf: the `yield_now` and
  stealing wakeups are gone). HTTP A/B at 256 clients, idle machine, 6 rounds: with the waker but no
  export thread 4.6% of request spans were lost; with both, none (27.2k req/s, p99 21.0 ms). Hostile test
  `a_self_waking_guest_yields_its_thread_at_each_tick` (three guest tasks waking themselves in a loop):
  a neighbour's requests still get through in under 50 ms each and the waker is killed at its CPU
  budget; removing both bounds fails it (the neighbour waited 533 ms).
- **Trade-off.** In-process under saturation, p99 rises (warm GET c256 ~15 → 22 ms; neighbours 2.6 →
  4.0 ms) while p50 and throughput improve; on the HTTP path p99 falls. A variant that yields between
  queued requests was tried and did not help.
- **Pick.** `4c37fad` and `932be36`, after H2. The runtime hunks apply; the test hunks touch the
  probe component and integration tests, which wave 1 reshaped (D1, S5): take them in branch order or
  resolve by keeping both probe routes. Runtime +49 and +17.

### H4. Link sockets are registered for reads only (N-41)

- **Problem.** tokio registers each `UnixStream` for read and write readiness, and a Unix stream
  socket signals writability whenever its peer reads, so each request's read woke the other process.
- **Change.** `link.rs`: a small `Io` over `AsyncFd`, registered for reads; a write that finds the socket
  full waits on a duplicate descriptor registered for writes, dropped once it goes through. Two
  `unsafe` blocks: tokio 1.54 made `AsyncFd`'s registration `unsafe`, and reads fill tokio's uninitialised
  buffer through rustix.
- **Evidence (loaded).** One client: context switches supervisor 2.59 → 2.02, worker 2.03 → 1.18; CPU
  452 → 375 µs. `bodies_larger_than_the_socket_buffer_go_through` (4 MiB up, 8 MiB back) stalls if the
  full-socket wait is registered for reads.
- **Pick.** `fb7055d`, after S5 (`1ee35ae 98c1698`) — applies cleanly. Runtime +64: the costliest
  wave-2 change per line; optional.

### H5. The supervisor's control runtime has its own pool of links (N-40)

- **Problem.** The readiness probe ran on the control runtime but put its link in front thread 0's pool,
  so front thread 0 served requests over a socket the control runtime's reactor drove.
- **Change.** The control runtime uses pool index `threads` (`Dialer::new(requests, threads + 1)`).
- **Evidence.** Control thread wakeups 1.90 → 0.03 per request on a fresh supervisor at one client.
- **Pick.** `e88acd5` — applies cleanly. Runtime +1.

## Wave 2: app cold start

Scale to zero means the first request to an app whose worker isn't running matters. Measured on the
production path (dev-mode supervisor, jailed worker, loopback registry); notes:
[notes/cold2.md](notes/cold2.md).

### C3. A new worker reuses the empty scratch directories the last one left (N-150)

- **Problem.** A worker that was killed, retired or switched leaves one `/tmp` directory per live
  instance; the next worker removed them all before its first request, ~3 ms each on this disk.
- **Change.** Empty ones are handed out again (as C1 does within a worker); used ones are removed as
  before; new names start past those left.
- **Evidence (loaded).** First request after the worker served 1,000 objects and was killed: 1,211 →
  51 ms; after 200: 119 → 37 ms. `scratch_left_by_a_previous_run` fails without it.
- **Pick.** `e082357`, after C1 (`f1e89d6 e082357` applies cleanly). Runtime +10.

### C4. The next worker is spawned ahead, and the jail checks it has one thread (N-151, N-152)

- **Problem.** Of a ~21 ms app restart, 11 ms was app-independent: fork and exec, and building the
  engine (wasmtime's stack pool takes one `mprotect` per slot).
- **Change.** A worker reads the node's config, builds its engine and linker (no guest input), and waits
  for an app; the supervisor keeps one such spare and spawns the next after each start. A spare gets no
  app, uid, cgroup or jail until taken; it exits when its supervisor does. The jail now refuses a process
  with more than one thread instead of relying on a comment — which revealed that the jail test's probe
  ran two threads and jailed only one (N-152, tests only); it now forks a single-threaded child.
- **Evidence (loaded).** Restart after a kill 21.5 → 10.0 ms (worker CPU on the path 20.1 → 8.1 ms);
  LRU retirement 34.2 → 18.0 ms; a switch with a cached compile 24.1 → 15.8 ms. Tests
  `workers_are_spawned_ahead`, `the_jail_needs_one_thread`.
- **Pick.** `99d0f55` — written on top of wave 1; take it in branch order. Runtime +54. A spare costs
  ~2.2 MB of memory per node.

### C5. A worker opens its cached component instead of receiving and re-hashing it (N-153)

- **Problem.** Every worker start piped the whole component through stdin and hashed it to find its
  cached compile, though the registry had checked it against its digest.
- **Change.** The registry hands over the component's digest and cache file; the worker keys its compile
  cache by the digest and reads the file only on a compile miss. The compile cache stays per app.
- **Evidence (loaded, on top of C4).** JavaScript app (5.4 MB): 78 → 16 ms, worker CPU 40 → 7 ms;
  counter 10.6 → 9.0 ms. `a_cached_compile_is_used_without_reading_the_component`.
- **Pick.** `77497df`, after C4. Runtime +21.

## Wave 2: smaller, and fixes found on the way

Notes: [notes/lean2.md](notes/lean2.md). Lean2 targeted getting the runtime back under 5,346 lines and
got from 5,652 to 5,466 before wave 2's features landed; it says so plainly.

- **K1** `ad0ccaa` — opening a kv bucket no longer creates its directories, so a request that only reads
  pays no journaled create and commit, and a new object doesn't spend two of its `max_files` on empty
  directories (N-110). JS counter's first GET p50 32 → 1.4 ms, CPU per cold start 4.7 → 1.6 ms (loaded).
  `reading_a_new_bucket_makes_nothing`. Needs `659bdbb a26bfa7` for its test hunks. Runtime +3.
- **K2** `62c5104` — a journal commit failed with `ENOTDIR` when a touched path's parent had become a
  file, so a guest could make its own commits fail (N-130). `a_path_under_what_became_a_file_commits`.
  Applies cleanly. Runtime 0.
- **K3** simplifications, each behaviour-preserving with the suite green (runtime Δ in brackets):
  `760b3e0` egress helpers (W-41) [−12, after D2 `1392e99`], `238bf82` the same helpers in supervisor and
  table [−12], `0e23bbe` quota entry-limit check [−5], `cb0e049` config defaults [−6], `e205632` prost
  varints [−19, after S6], `d8e3a9f` journal `or_poison` [−5], `cefdfcd` quota writer without `Drop`
  [−7], `cf16c73` registry I/O through `tokio::fs` [−9], `9a6b99b` one `cancelled` guard [−3, after
  `760b3e0`], `9b39f93` ws publish check [−3, after S3], `c356402` one proxy serve loop [−2, after L5
  `29c8ddd`], `c898bee` `delegate!` for two stock calls [−5], `7f7f18d` `Default` for two structs [−11],
  `1872b6a` journal `iter::from_fn` [−3]. Those marked "applies cleanly" in testing: `0e23bbe cb0e049
  d8e3a9f cefdfcd cf16c73 c898bee 7f7f18d 1872b6a 238bf82`; the others need the commit named, or branch
  order.
- **K4** `aa2d770`, `1122832` — test-only code (the counting allocator, `ObjState::for_test`,
  `server::ok_json`, quota's `usage()`) moved below the line CI counts. This corrects the count by 87
  lines; it simplifies nothing.
- **K5** `e27f59a` (N-91: wait for the in-flight requests instead of sleeping 200 ms; applies cleanly)
  and `6cf4798` (N-132: wait for the directory, after C1).

## Maintenance

- **M1. wasmtime 50.0.1** (`4e86bc8`): a security release (five GHSA advisories, among them unchecked
  memory use when borrowed resources move between components). The commit only touches `Cargo.lock`
  and conflicts with `main`'s lockfile; on `main` run instead:
  `cargo update -p wasmtime --precise 50.0.1` and the same for `wasmtime-wasi`, `wasmtime-wasi-http`,
  `wasmtime-wasi-io`, `wasmtime-wasi-config`. Verified: full suite passes on 50.0.1.
- **M2. Benchmark instrumentation**: `3c0799d` (CPU per request on warm rows) and `57cc0f7 7dabdaf`
  (`--only`, `--conc`, CPU per cold start, fds/maps/heap per object). Each set applies on its own;
  together they conflict in `bench.rs` (take `crates/nanovm/examples/bench.rs` from this branch).
- **M3. Tests**: `dd2efa4` gives a proxy test its own publish port (N-92).
- **M4. Docs**: `8e73172` (README front door), `2ee94bc` (notes index), `0d5f783` (PLAN/HANDOFF). They
  describe the whole branch; edit them down to what you take.

## Not done, and why

- **W-8, comparison against Spin, `wasmtime serve` and workerd.** It needs downloading and running
  third-party runtimes, which this environment's permission policy refused. Nothing was worked around.
- **macOS.** nanovm no longer compiles on macOS (rustix's Linux-only link flags, N-131). It was found,
  not caused, here; fixing it needs a Mac to verify, and whether macOS stays supported is the owner's call.
- **Runtime size.** The runtime ends 6% larger than it started (5,346 → 5,687), net of 137 lines removed
  by simplification (L3–L5, K3) and 87 lines of tests no longer counted; the growth buys the features and speed in
  the table at the top. W-60/W-61 list further candidates.
- **Open findings** worth a look next: N-131 (macOS), N-132 (store counted gone before its `/tmp` is
  released), N-140 (JS recursion is a trap), N-154/N-155 (app switch and worker start leftovers), W-20
  (thread-per-core, not built: its main motivation is gone), W-35 (proxy-wide buffer bound, designed).
- **One unexplained HTTP 500** in 1.2M requests at 256 clients on the final build (likely a CPU-budget
  kill under saturation; the budget is wall time inside polls, which inflates when oversubscribed).
