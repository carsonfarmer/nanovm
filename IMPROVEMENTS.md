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

## Verification of the whole branch

| Check | Result |
|---|---|
| Full locked suite, serial, idle machine (`env -u RUST_BACKTRACE RUST_TEST_THREADS=1 cargo test --locked -p nanovm`) | 115 unit, 74 integration (10 ignored), 9 JavaScript: all pass |
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
| J1 | JavaScript apps (`fetch(request, env)`) | apps in JavaScript; zero runtime lines | 0 | yes (wave 2 updates it) |
| **Maintenance** | | | | |
| M1 | wasmtime and WASI crates 50.0.1 | five security advisories fixed upstream | 0 | yes (regenerate the lockfile) |
| M2 | bench prints CPU per request, fds/maps/heap per object | A/B on a busy machine | 0 | yes |
| M3 | test fixes: free publish port (N-92) | no failures when port 8444 is taken | 0 | yes |
| M4 | docs: README front door, notes, PLAN/HANDOFF | clearer | 0 | with the changes they describe |

Wave 2 (in progress at the time of writing) adds entries below the wave-1 ones.

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
- **Pick.** `a26bfa7 c63b06f 9cde8c2 95d3212 70f9516 050885b` — applies cleanly. Wave 2 (Q1 below)
  replaces the engine; take its commits on top.

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
- **The production HTTP path did not get faster in wave 1** (idle A/B: 17.5k vs 17.4k req/s, 178 vs
  176 µs CPU per request). Wave 2's hop workstream targets it.
