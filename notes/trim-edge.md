# Trim pass: edge (`trim/edge`)

Files: `crates/nanovm/src/` object, registry, egress, ws, proxy, server, link, jail, gate, engine,
metrics. Base `origin/beta/integration` 7988cae. Owner-approved removal: request-rate limits
(HANDOFF Q91). Targets: must reach ≤ 2,960, aim ≤ 2,612.

## Result

CI's count (non-blank, non-comment lines before `#[cfg(test)] mod`), and, so the savings can't be
layout, the same files formatted with the repo's `rustfmt.toml` in a temp copy (base and branch
alike; the tree itself is not reformatted).

| File | Base | Now | Formatted base | Formatted now |
|---|---|---|---|---|
| object | 801 | 773 | 891 | 855 |
| registry | 482 | 467 | 633 | 615 |
| egress | 443 | 398 | 535 | 502 |
| ws | 431 | 418 | 530 | 517 |
| proxy | 412 | 396 | 606 | 579 |
| server | 193 | 172 | 218 | 203 |
| link | 189 | 183 | 209 | 203 |
| jail | 165 | 165 | 191 | 191 |
| gate | 145 | 79 | 184 | 101 |
| engine | 138 | 131 | 142 | 139 |
| metrics | 83 | 78 | 83 | 78 |
| **edge** | **3,482** | **3,260** | **4,222** | **3,983** |
| runtime total | 8,711 | 8,480 | 10,643 | 10,395 |

−222 (−6.4%); −74 of it is the owner's rate-limit removal. **Must-reach (2,960) is not met**: what
is left is behaviour, security or measured performance (see "What remains, and why"). The
runtime total also counts −7 in supervisor.rs and −2 in config.rs (rate-limit call sites).

## Duplication found

"Here": removed on this branch. "Coordinator": the other copy is in another agent's file.

| What | Where (both places) | Lines | Done |
|---|---|---|---|
| Bodiless answer with a status | server::admin ×4 (+ `full`), egress::hold, egress::serve_forwarded, proxy::publish; supervisor::admin, supervisor::verdict_ok, supervisor::get, main::print | ~15 | here: `object::empty_response`; supervisor/main: coordinator (≈ −4) |
| Keep a guard alive with a response body (`map_frame` closure) | egress::send_request, egress::call; supervisor::forward | 7 each | here: `object::holding`; supervisor::forward: coordinator (−7) |
| Connection slots held by capturing them in per-request closures, or by a second task per connection | gate::serve (2 branches), link::serve | 9 | here: `server::serve_conn` takes a guard |
| A connection's peer address read twice | server::accept dropped it, gate::serve asked `peer_addr()` again | 1 + a syscall | here |
| Write aside and rename into place, unique tmp name | engine::load_component, registry::fetch | 9 | here: `engine::write_whole` |
| Blocking cache read by hand | registry::fetch `spawn_blocking(std::fs::read)` | 6 | here: `tokio::fs::read` |
| Body cap by hand | object::GuardedBody counted bytes as `http_body_util::Limited` does | 10 | here |
| Waker slot by hand | object::SelfWake `Mutex<Option<Waker>>` + `will_wake` = `futures::task::AtomicWaker` | 6 | here |
| rlimits through `unsafe` libc with hand-built structs | engine::raise_nofile, engine::preflight | 10 | here: rustix `process` (no new crates) |
| A monotonic ns clock | metrics::now_ns (Instant since start), lease::mono_ns (CLOCK_MONOTONIC) | 5 | here: object.rs calls `lease::mono_ns` |
| `clock_gettime` by hand | metrics::thread_cpu_ns, lease::mono_ns | 4 | coordinator: one helper (storage owns lease.rs) |
| Egress hooks' copies of node state | `EgressHooks.route` = `node.app.egress`, `.metrics` = `node.metrics` | 6 | here |
| IPv4-mapped handling by hand | egress::is_global → is_global_v4 | 5 | here: `to_canonical` |
| `nanovm-*` and hop-by-hop header stripping, two collect-and-remove loops | proxy::forward | 3 | here; proxy::call keeps its own (it keeps `nanovm-callers`) |
| `ws::Reply` enum shaped like `Result<Answer, u16>` | ws, proxy deliver, main::event | 8 | here: a type alias |
| `ws::well_formed` = `messages(..).is_some()` | ws, egress::hold | 4 | here |
| `ObjShared::holds` = `lease_epoch().is_ok()` | object.rs | 1 | here |
| Test support in the runtime | `server::ok_json`, used only by tests | 7 | here: tests have their own |
| Rate limiting (owner removal) | gate.rs, proxy, supervisor, config | 74 | here |
| Request span attributes (method, url.path, truncated) | egress::send_request, table::dispatch, telemetry::forward | 2–4 each | coordinator: one helper in telemetry |
| Trace context injection | egress::send_request, egress::call, telemetry::forward | 3 each | coordinator (telemetry) |
| Body collection with `Limited` and status mapping | server::handle, egress::call/hold, ws::publish/answer, proxy::send (a loop), telemetry::Relay | 4–12 each | left: a shared `collect` over the proxy's boxed body trips rustc's "`From` is not general enough" Send check, and a hand loop for the node would copy every request body (Limited+collect keeps a one-chunk body zero-copy) |
| Accept loops | server::serve_http1, gate::serve, proxy::serve_publish, link::serve | 3–6 each | left: limits, header timeout and fd passing differ |
| `x-nanovm-egress` verdicts | egress::forward/serve_forwarded; supervisor::call, verdict_ok, telemetry and lease routes | ~10 | coordinator: `verdict_ok` = `empty_response` + insert |
| In-process socket delivery | main::local/event; proxy::socket's deliver | ~10 | coordinator (main.rs is node's) |
| The volume stat'ed twice per instantiation | object::new_store `symlink_metadata`; journal::Journal::open `metadata` | 1 + a syscall | coordinator: Journal::open could refuse a non-directory itself |
| Durable write by `written` | ship::swap and friends | — | not a duplicate of `write_whole`: ship's syncs; the cache's needn't |

## Other candidates (generality, checks, types, knobs)

Done:
- `[clients] rate`, `app_rate`, `governor` (+ nonzero_ext, spinning_top, futures-timer), `Peer`,
  `Gate::client`/`admit`, `X-Forwarded-For` reading: removed (owner). `trusted` stays: the
  per-client connection cap must not count a TLS terminator, or the proxy in front of a node, as
  one client.
- proxy::socket's `apps.len() >= max_sockets`: every socket counts toward `max_sockets` in
  `Sockets::open` before it waits, and an app stays in the map only while something holds its
  sockets, so the map can't outgrow it.
- egress::forward stripped a timeout header the guest set itself before setting the options: the
  supervisor reads either as the same capped option.
- `begin_request` (wrapper used once), `drop(theirs)` in link (closed at the end of `checkout`
  anyway), std socketpair + `set_nonblocking` + `from_std` (tokio's `pair` is that).

Not done, for the owner or coordinator:
1. **macOS.** `supervisor.rs` uses `libc::CGROUP2_SUPER_MAGIC` unconditionally, which libc defines
   only for Linux, so the crate doesn't build on macOS today; PLAN still says the portable core runs
   there. Either fix that or drop the macOS branches (here: jail's non-Linux `enter`, metrics'
   macOS `rss_bytes`, about −18; more in journal.rs).
2. **`strum`** (already built for oci-spec) could derive `Phase`/`Poison`'s `ALL` tables and names:
   about −12, a direct dependency and a call-site edit in table.rs.
3. **Calls carry `traceparent` in headers** like outbound requests, instead of a context extension
   on single-node calls: −7 in egress (and a dead branch in table::dispatch), but a callee would
   then see a `traceparent` header it doesn't see today.
4. **Push uploads as POST + PUT with the body** (OCI monolithic upload) instead of POST, PATCH, PUT:
   −1 to −3, needs a check against GHCR.
5. engine::start_ticker's `catch_unwind` + abort guards a panic that can't happen, but it is the
   CPU budget's backstop: kept.

## What remains, and why

- **object (773):** the actor and its state (CPU charging per poll, leases, phases), `MemLimiter`
  (caps the *sum* of an instance's memories; wasmtime's `StoreLimits` caps each), `LogTail` (trap
  messages, guest log export), `Scratch` (measured cold-start reuse), the store (WASI, quota,
  epoch callback with heavy lanes), `commit` (lease-gated, held publishes), `Pending` (the reply
  the supervisor answers on a kill, per-phase spans), `run` (restore pause, leases, slots,
  `remote_ack`, instantiate errors), `serve` (two commits, deadlines per phase), `SelfWake`
  (measured scheduler optimization), `classify`, `GuardedBody`.
- **registry (467):** OCI types; `from_headers` with its measured parse cache; `app_name`;
  load/authorize/verify (Sigstore); the hardened client (bounded reads; private addresses refused
  at the resolver, on redirects and in URLs and token realms); push for `nanovm publish` (owner:
  no oci-client).
- **egress (398):** policy parse and match; `is_global`; the hook (publish, call and outbound
  routing, spans, metrics); `call` (cycles, the leases path); `hold`; the worker↔supervisor
  protocol (`forward`, `serve_forwarded`, `error_named`); `send` (connects to the checked address,
  TLS, per-request timeouts).
- **ws (418):** the WebSocket-over-HTTP wire format, `open` (handshake, caps, OPEN round trip,
  GRIP), `run` (one event in flight, backlog, ping, stall), GRIP publish.
- **proxy (396):** the route/digest cache (single-flight, serve-stale, bounded), forwarding with
  failover, sockets, the private publish/call endpoint, DoH lookup and TXT parsing, rendezvous.
- **server (172), link (183), gate (79):** accept, `Paced` (N-505), `serve_conn`, the node's
  routes; fd passing and pooled HTTP/1 over socketpairs; connection caps.
- **jail (165):** Landlock and the seccomp allowlist, each entry a traced need.
- **engine (131), metrics (78):** engine config, preflight, ticker, compile cache; counters,
  poison names, RSS.

## Tests and mutation checks

Full suite (`env -u RUST_BACKTRACE RUST_TEST_THREADS=1 cargo test --locked -p nanovm`) on the
branch: lib 192 passed (1 ignored), configs 1, dev 11, examples 29, integration 127 (14 ignored),
js 17, committed_state 2. Production check `cargo check --locked -p nanovm --bin nanovm` passes.

Changed or new tests, each mutation-checked (mechanism broken, test fails, file restored with
`git checkout --`, `git diff` empty):
- `gate::connections_are_capped_per_client` (adds IPv4-mapped and trusted-range cases): fails
  without `to_canonical` in `key`, and without it in the trusted check.
- `response_bodies_are_capped` (new, integration): fails with `Limited::new(b, usize::MAX)` in
  `object::serve`.
- `object::guarded_body_caps_and_aborts` now checks for `LengthLimitError`: the cap is `Limited`'s.
- Removed with the rates: `clients_past_their_rate_are_refused_however_they_forward`,
  `apps_past_their_rate_are_refused`, `clients_are_named_only_by_trusted_forwarders`,
  `rates_are_per_client_and_per_app`.
