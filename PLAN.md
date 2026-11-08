# nanovm: plan for the portable core

## Context

**Current priority (2026-11-08):** offline backup and N-43 are delivered and tested.
Next is W-9 committed-state capture/transfer and recovery groundwork, then W-10
placement/fencing. This sequence is not approval of a replication or failover policy.
Runtime remains 5,085 lines; the under-5k goal is unmet. Historical experiments below
are not instructions to restart rejected actor/page-backend rewrites.

The goal is to run large numbers of untrusted wasip3 HTTP components at high density. Each one must be strongly isolated with hard limits.

The run model is shaped like Durable Objects:
- Each `(app, object-id)` pair has exactly one live instance.
- Requests to that instance are served one at a time.
- In-memory state is lost when the instance is evicted.
- Each object has its own persistent disk.

Production targets Linux cloud VMs without KVM. Phase 1 was the **portable core**, which runs natively on macOS and Linux. Phase 2 adds the Linux jail (a process per app with seccomp, Landlock and cgroup v2); see below.

The stack is wasmtime 50.0.0 with wasmtime-wasi 50 and wasmtime-wasi-http 50 (P3 support enabled by default). P3 is marked experimental upstream, with no semver guarantees.

Done means:
- `nanovm serve` hosts real guest components with every limit enforced.
- `cargo test` proves each guarantee against hostile guests.
- `examples/bench.rs` prints cold-start, warm-latency, throughput and density numbers.
- `demo/demo.sh` drives thousands of objects and the attack storm over HTTP. Observability uses OTLP; the demo reads a local collector's JSON file. The node has no stats/events HTTP APIs.

## Architecture

```
client ──HTTP──► nanovm (hyper 1, http1)
                  │  data:    /{object}/{rest...}, app named by headers (guest sees /{rest...})
                  │  telemetry: OTLP through the supervisor to a collector
                  ▼
              ObjectTable  (app, object) → ObjectHandle { mailbox tx, last_used, state }
                  │  on miss: spawn actor; at max_live_objects, evict the LRU idle object first
                  ▼
              Object actor: one tokio task per live object; owns Store<ObjState> + Service
                  • bounded mailbox; strictly one request at a time
                  • phases: Handling → PostReturn → Idle, each with a deadline enforced
                    outside run_concurrent
                  • CPU budget per request (epoch ticks), memory cap (custom ResourceLimiter)
                  • trap / timeout / limit → poison → drop Store (/data survives),
                    then re-instantiate for queued requests
                  │
                  ├─ VFS (wasi:filesystem@0.3, quota'd):  /tmp scratch · /data persistent
                  ├─ env: NANOVM_APP, NANOVM_OBJECT_ID (the object's "this.id")
                  └─ egress: allowlist → DNS → public-IP filter → connect to the resolved IP
```

## Layout

```
nanovm/
  Cargo.toml                     host workspace
  crates/nanovm/
    src/lib.rs                   module list; the API is Node::new(cfg), Node::dispatch(app, id, req)
    src/config.rs                NodeConfig, AppConfig, Limits (TOML via serde)
    src/engine.rs                Engine (pooling, epoch ticker), .cwasm cache, deserialize_file
    src/app.rs                   load component, validate imports, build Linker + ServicePre
    src/object.rs                actor loop, phases, deadlines, limiter, CPU accounting, heavy lanes, poison
    src/table.rs                 Node: (app, id) -> actor, slot permits, LRU/idle eviction, retire handshake
    src/quota.rs                 quota'd wasi:filesystem@0.3 host (QuotaFs), per-object disk budget, QuotaWriter
    src/egress.rs                allowlist, IP classifier, resolver + connector, send_request
    src/metrics.rs               infrastructure atomics and RSS
    src/server.rs                hyper front end, routing, private readiness/flush
    src/main.rs                  `nanovm serve --config demo/nanovm.toml`
    examples/bench.rs            in-process benchmark driver (no sockets)
    tests/integration.rs         integration tests against real guest components
  guests/                        separate workspace, target wasm32-wasip2, cdylib + wasip3 0.9
    counter/                     benign DO: in-memory hits + persisted counter + kv in /data
    hostile/                     attack endpoints (see Tests)
    util/                        shared request/response and wasip3 filesystem helpers
  crates/nanovm/support/load.rs  in-process load generator, #[path]-included by bench and tests
  scripts/build-guests.sh        cargo build --target wasm32-wasip2 --release → guests/target/wasm32-wasip2/release/
  demo/nanovm.toml, demo/demo.sh
```

## Engine and instantiation (src/engine.rs, src/app.rs)

- **Config:** `epoch_interruption(true)`, `max_wasm_stack` and `async_stack_size`. Component-model-async is on by default in 50.0.0.
- **Pooling allocator,** all configurable:
  - `total_component_instances` = max live objects
  - `total_memories`, `total_tables`, `total_core_instances`
  - `max_memory_size` = memory cap per object
  - `memory_reservation` and `memory_guard_size`
  - `max_unused_warm_slots`, `decommit_batch_size`

  With the default 4 GiB reservation, each memory slot takes about 4 GiB of virtual address space. Density mode uses a small reservation instead (for example 128 MiB). It costs explicit bounds checks but gives roughly 100k slots. The benchmark measures both modes.
- **Precompiled cache:** components are precompiled to `.cwasm`, keyed by content hash plus `precompile_compatibility_hash`. They are loaded with `Component::deserialize_file`, which is required for copy-on-write memory images on macOS; Linux uses memfd.
- **Deploy-time validation (`ALLOWED_IMPORTS` in app.rs):** a component is rejected if `component_type().imports()` contains anything outside these prefixes: `wasi:io/`, `wasi:clocks/`, `wasi:random/`, `wasi:cli/` (any version), `wasi:filesystem/{types,preopens}@0.3`, `wasi:http/{types,client}@0.3`. It must also export `wasi:http/handler@0.3`. Sockets, the WASI 0.2 filesystem, `wasi:http/outgoing-handler` and custom imports are all refused, and the error names the offending import.
- **WASI 0.2 filesystem:** refused at load (above). Rust guests built for wasm32-wasip2 must use the wasip3 filesystem API (see `guests/util`), not `std::fs`, which imports the 0.2 filesystem. Quota support for 0.2 is a follow-up.
- `ServicePre::new(linker.instantiate_pre(&component))` is built once per app and cloned for each instantiation.

## Object actor (src/object.rs)

- Each object has its own loop, modeled on wasmtime-wasi-http's `Worker::run_`. We can't use upstream `ProxyHandler`, because it has a single shared queue and no object affinity. The loop body:
  ```
  store.run_concurrent(async |acc| loop {
    recv request
    → p3::Request::from_http(hooks, req)
    → svc.handle(acc, req)
    → res.into_http(..)
    → wrap body
    → reply
    → poll_fn(poll_no_interesting_tasks)        // strict serial
  })
  ```
  `from_http` takes scheme and authority from the URI only, so `server.rs` rebuilds the URI as `http://{Host}{path}` before dispatch. The request body is collected (capped at `max_request_body_kb`) before it reaches the actor.
- **Deadlines:** the run_concurrent future is wrapped in an outer `poll_fn` that reads a shared `(phase, since)`. On expiry it drops the future and the Store. Upstream documents timeouts inside run_concurrent as unreliable.

  | Phase | Deadline | On expiry |
  |---|---|---|
  | `Handling` | `request_timeout` | 504 |
  | `PostReturn` | `post_return_timeout` | the streaming body is aborted with an error frame, so a truncated response doesn't look complete |
  | `Idle` | `idle_timeout` | clean eviction |
- **CPU budget:**
  - The outer `poll_fn` times every poll of the store's event loop and charges it to the current request. That covers guest code and inline filesystem calls; it is wall time, so it inflates when the host is oversubscribed.
  - An epoch ticker thread runs every `epoch_tick_ms` (default 5 ms). Each Store's `epoch_deadline_callback` checks the charge including the poll in progress, and past `cpu_ms` returns an `Err(CpuExceeded)` trap.
  - Otherwise it returns `Yield(1)`, so a spinning guest still yields to tokio.
  - **Heavy lanes:** once a request has used 10 ms (`HEAVY_AFTER_NS`), the callback returns `YieldCustom`, which waits for one of `heavy_lanes` permits before the guest runs again. The permit is handed back at every tick. This stops CPU hogs from yielding straight back onto every worker.
  - The Store is per object and serves one request at a time, so per-store accounting is per-request accounting.
- **Memory:** a custom `ResourceLimiter` holds the total across *all* linear memories of the object; upstream `StoreLimits` is per memory. Growing past it returns `Err(MemoryLimitExceeded)`. Table elements, instance count and table count are also capped.
- **Other per-store caps:** `ResourceTable::set_max_capacity(max_resources)`. Header field size uses the `WasiHttpCtx` default (128 KiB).
- **Poisoning:**
  - Any `Err` from run_concurrent poisons the Store upstream, so we drop it.
  - The pending reply gets 500 or 504 with an `x-nanovm-error` reason: `cpu`, `memory`, `timeout`, `trap`, `resource-table`.
  - `/tmp` is wiped, `/data` is kept, and the event is logged.
  - If requests are still queued, a fresh instance takes over.

## Object table (src/table.rs)

- `HashMap<(AppId, ObjectId), Entry>` behind a mutex. Object IDs match `[A-Za-z0-9_.-]{1,128}` and map to path-safe directories.
- **Send path:** clone the sender under the lock, then send. If the channel is closed (the actor just exited), respawn and retry once.
- **Idle exit handshake:** the actor takes the lock, confirms its mailbox is empty, then removes its entry and exits.
- **Capacity:** the table holds at most `max_live_objects` objects. A new object past that evicts the LRU *idle* object: its entry is removed, so its actor drains and exits. If nothing is idle, the request gets 503 `node-at-capacity`.
  - An evicted store lives until its actor next runs. So every store holds one of `max_live_objects` slots (`Node::slots`), acquired before `new_store` and released only after the store is dropped, and the pool has room for all of them. A new object waits for a slot instead of finding the pool full. `churn_past_capacity_never_fails` guards this through `stores_peak`.
- **Full mailbox:** 503 with `x-nanovm-error: busy`.

## Quota'd filesystem (src/quota.rs)

`quota.rs` links its own `wasi:filesystem/{types,preopens}@0.3` host, `QuotaFs`, in place of the stock one (no linker shadowing). Every function delegates to the stock `WasiFilesystem` implementation through `Accessor::with_getter`; the ones that can consume bytes or directory entries on a quota'd mount are checked first. No fork is needed.

`allow_blocking_current_thread(true)` is set on the `WasiCtxBuilder` before the preopens (which capture it), so small filesystem calls run inline and are charged to the CPU budget instead of queueing on tokio's `spawn_blocking` mutex.

- **Preopens:** two calls to `preopened_dir(host, guest, FsPerms)` (the read-only `/app` went with local apps in phase 7):
  - `/tmp` read-write, at `scratch/<app>/<object>-<instance>`
  - `/data` read-write, at `data/<app>/<object>`
- **One budget per object:** `/data` and `/tmp` share `disk_mb` and `max_files`, so no descriptor-to-mount bookkeeping is needed. `/data` is walked at instance start; `/tmp` starts empty.
- **Writes:** `write-via-stream` and `append-via-stream` use our `QuotaWriter: StreamConsumer`, which writes inline: the quota check and `write_at` happen under the quota lock, writing as much as the budget allows, so a filler sees a short write and then an error, as on a full disk.
  - Going over the byte budget returns `insufficient-space`.
  - Going over the entry limit returns `quota`.
  - When dropped before a result is sent, it sends `Ok(())`.
- **Pre-checked:** `open-at` with CREATE (entry count) or TRUNCATE (credits the old size), `create-directory-at`, `set-size`.
- **Credited back:** `unlink-file-at`, `remove-directory-at`, and `rename-at` when it replaces a *different* existing file. A rename onto the same file (compared by `metadata_hash_at`) is a POSIX no-op and credits nothing.
- **Denied:**
  - `link-at` and `symlink-at`, so shared inodes and link targets don't have to be tracked.
  - Renames between `/tmp` and `/data` are allowed: both draw on the same budget.
- **Everything else** delegates to `<WasiFilesystem as HostDescriptorWithStore<T>>::*`.
- **Counting:** logical `st_size`. Files, directories and symlinks all count toward the file limit.
- **Path escape:** stock wasmtime-wasi already confines paths to their preopen. Tests confirm it.

## Egress (src/egress.rs)

`WasiHttpHooks::send_request` is the single choke point for outbound requests, and we override it.
1. **Allowlist:** checked against `uri.host()` and `port_u16()` (never the raw authority). Entries are `host`, `host:port`, `*.suffix[:port]` or an IP literal. Scheme and method are not matched yet. No match returns `HttpRequestDenied`.
2. **Concurrency:** a per-object semaphore. The permit is held inside the returned I/O future. When exhausted, the request fails with `ConnectionLimitReached`.
3. **DNS:** `lookup_host` under a timeout. Every non-public address is dropped; if none remain, the request fails with `DestinationIpProhibited`. Dropped ranges:
   - loopback, RFC1918, CGNAT, link-local (including 169.254.169.254)
   - ULA, multicast, unspecified, documentation and benchmark ranges
   - IPv4-mapped and NAT64 forms of all of these

   An IP-literal allowlist entry bypasses the filter, since the operator named the address explicitly. Every resolved address must be public; a name that mixes in a private address is treated as rebinding.
4. **Connect** to the resolved `SocketAddr`, which defeats DNS rebinding. TLS uses rustls with webpki roots (the config is currently rebuilt per request). The client is hyper http1. Guest-set connect and first-byte timeouts are capped at 10 s and 30 s.
5. **Not yet done:** a timeout on the TLS handshake itself, a between-bytes timeout and a size cap on the upstream response body. Today these are bounded only by the request's wall-clock deadline.
6. **Sockets:** wasi sockets stay denied by default, and `inherit_network()` is never called.

## Limits (per app, from its `nanovm.json`, clamped to the node's `[limits]`)

| Knob | Default | Enforced by |
|---|---|---|
| `memory_mb` | 64 | custom ResourceLimiter (all memories) + pool `max_memory_size` |
| `cpu_ms` per request | 200 | wall time inside store polls, checked by the epoch callback |
| `request_timeout_ms` / `post_return_timeout_ms` / `idle_timeout_ms` | 5000 / 5000 / 30000 | outer deadline wrapper |
| `max_request_body_kb` / `max_response_body_kb` | 1024 / 8192 | body wrappers |
| `disk_mb` / `max_files` | 64 / 1000 | QuotaGuard (shared by `/data` and `/tmp`) |
| `max_queue` | 64 | bounded mailbox |
| `max_outbound_concurrency` | 4 | egress semaphore |
| `max_resources` | 10000 | ResourceTable + component handles |
| `egress` | `[]` (deny all) | allowlist |

Node-level settings: `max_live_objects` (1000), `memory_reservation_mb` (128), `listen` (127.0.0.1:8080), `data_dir` (nanovm-data), `epoch_tick_ms` (5), `heavy_lanes` (cores that requests past 10 ms of CPU may occupy at once; 0 means half the cores).

## Guests (guests/, wasm32-wasip2 + wasip3 0.9 + `async-spawn`)

`rustc`'s bundled `wasm-component-ld` produces the component directly, with no adapter or wasm-tools needed.
- **counter** (benign object):
  - `GET /`: object id, in-memory hit count, persisted counter
  - `POST /incr`
  - `GET/PUT/DELETE /kv/{key}`, stored in `/data/kv`
- **hostile:**
  - `/alloc?mb=`, `/spin`, `/sleep?ms=`, `/panic`
  - `/fill?mount=&mb=`, `/files?n=`, `/bigfile?mb=`
  - `/write-app`, `/escape`, `/cross-rename`
  - `/fetch?url=`
  - `/leak?n=` (holds handles in a static)
  - `/concurrency`: reports the maximum overlap observed
  - `/echo`

## Tests (crates/nanovm/tests/integration.rs, real components)

CI runs on pushes and pull requests (W-16, done and verified on GitHub): build the guests locally, check the production binary without dev features, then run the locked test suite. It does not publish packages or deploy. Serial test scheduling reduces shared-runner CPU contention without serializing requests inside concurrency tests.

The max-app admission fixture holds observed upstream requests until explicit release, rather than assuming guest sleeps outlast cold startup (N-43). Its request deadlines are test-only; production budgets are unchanged.

Each test spins up a `Node` in-process with small limits.
- **Basics:** routing, `NANOVM_OBJECT_ID`, in-memory state persists within an instance, objects are independent of each other.
- **Serial:** 50 parallel requests to one object produce a maximum overlap of 1. Mailbox overflow returns 503.
- **Memory:** `/alloc` over the cap gives 500 `memory`. The object restarts and a neighbor object is unaffected.
- **CPU:** `/spin` is killed at roughly `cpu_ms`, with 500 `cpu`. The latency of a concurrently running neighbor object stays low.
- **Wall clock:** `/sleep` longer than the timeout returns 504. The next request is served by a fresh instance.
- **Durability:** `incr` ×3, force an eviction, `GET` shows 3 and the in-memory count reset. Usage on `/data` is restored after a restart.
- **Quota:** filling `/tmp` or `/data` returns `insufficient-space`, `/files` returns `quota`, `/write-app` is refused, `/escape` (`..`, absolute paths, symlinks) is denied, and `/cross-rename` (from `/tmp` to `/data`) succeeds and keeps one budget. `/data` usage is rescanned after a restart. Renaming a file onto itself credits nothing, while a rename over another file frees it.
- **Egress:**
  - a host not on the list (`169.254.169.254`, `HttpRequestDenied`)
  - an allowlisted name (`localhost`) resolving to loopback, with a listener present (`DestinationIpProhibited`)
  - an allowed request to `https://example.com` succeeds (ignored by default: needs the internet)
- **Resources:** `/leak` past `max_resources` traps and poisons the object; the object recovers.
- **Capacity:** with `max_live` objects full, a new object evicts the LRU idle one.
- **Validation:** a component importing sockets is rejected at deploy time. *(Not written yet.)*
- **Leaks:** RSS and open fds stay flat over long runs and churn. *(Not written yet.)*

## Benchmarks (`examples/bench.rs`)

`cargo run --release -p nanovm --example bench -- [--quick] [--json out.json]`. Results are printed as a table; `--json` also saves them. The latest results are in [README.md](README.md).
1. Compile time and `.cwasm` load time.
2. Cold start: latency of the first request to a new object, p50/p99/max (instantiation plus first handle).
3. Warm requests: single object served serially, p50/p99, measured both in-process (`Node::handle`) and over HTTP.
4. Throughput: N objects × M requests, concurrency C, in requests per second.
5. Density: spawn K live objects (1k, 10k, and larger if address space allows), measuring RSS delta per object, time per object, and virtual address space reservation. Run with both the 4 GiB and small `memory_reservation`.
6. Churn: evictions plus re-instantiations per second.

## Demo

`demo/demo.sh` starts a node from `demo/nanovm.toml` (or uses one already on :8080) and drives it over HTTP with `curl --parallel`: per-object volumes and escapes, cold starts, warm requests, `/data` writes, and a storm of every hostile-guest attack at once, while a canary object is pinged throughout.

## Order of work (phase 1: portable core, done)

All nine steps are done. [README.md](README.md) has the results and the bugs the tests and benchmarks found.

1. Guests workspace plus a minimal `counter`; confirm the toolchain and inspect the component imports.
2. Engine + linker + one object actor serving end to end over HTTP.
3. Limits, deadlines, poisoning and re-instantiation, with metrics.
4. Quota'd filesystem.
5. Egress.
6. ObjectTable with LRU/idle eviction and the control API.
7. Integration tests.
8. Benchmarks.
9. Demo script and results.

## Target design (agreed)

The goal: one node, as fast and secure as we can make it, that runs untrusted WebAssembly apps as durable objects, deployable by anyone without handing out a long-lived secret. It is a solid single-node baseline to measure against before any clustering.

- **Placement:** the node sits behind a trusted proxy on a private network. The proxy is the public HTTP entry point (route → digest), behind a TLS terminator; only the proxy-to-node hop is private. The node has no TLS and no auth of its own.
- **Tenancy:** tenant = app, keyed by the DNS name that owns its record (after CNAMEs) and its repository (phase 7, W-15). Routes are `/{object}/{path}`, with the app named by the proxy's headers. The node exposes only data APIs.
- **Isolation, in layers:** Wasm with the per-object limits; one jailed worker process per app; a supervisor that holds no untrusted code and owns the network. A Wasm escape reaches one app's jail and nothing else.
- **Deploy, stateless and keyless:**
  - Apps are OCI artifacts (an `application/wasm` layer plus a config blob with limits and env), pulled anonymously and by digest only.
  - They are signed with Sigstore keyless signing: GitHub artifact attestations from CI, or `cosign` with a browser OIDC sign-in on a laptop. No long-lived keys exist anywhere.
  - The proxy sends, with every request, the manifest digest, the app's trust policy (a list of exact `{issuer, subject}` identities) and its minimum version.
  - The node fetches on first use (single-flight per digest), verifies offline with `sigstore-verify` against the embedded trusted root, compiles inside the app's jail and caches by digest. It keeps no deploy state and has no deploy API.
  - New requests go to the new version; objects on the old version drain and are evicted.
  - Manifest limits are clamped to operator maximums.
- **Features to add:**
  - **WebSockets** (done), following tric's WebSocket-over-HTTP model: each event (`OPEN`/`TEXT`/`BINARY`/`CLOSE`/`DISCONNECT`) is a normal bounded request to the object, and sockets outlive instances.
    - The front proxy speaks the protocol and handles channel publishing; there is no Pushpin process (Q55). The supervisor forwards committed publishes with its worker's app identity.
    - Events are ordered per object. Channels are scoped per object, as in Durable Objects (Q32).
    - Sockets survive worker exits and version switches; every event resolves the current digest (Q53, superseding Q35). A changed app identity closes the socket rather than crossing apps (Q54).
    - Proxy settings bound sockets globally, per app and per object, message/answer size and per-socket backlog (Q55, superseding Q36).
  - **Alarms** (Q30, then Q41): skipped so far; queued as a scheduler outside the node (Roadmap 4).
  - **Key-value** (done): composed Spin async KV stored as files under `/data`; no native host KV implementation. `wasi:filesystem` stays exposed. `wasi:config` serves each app's config values.
- **Working rules:** least code; fix root causes; never write backward-compatibility code; every limit is proven by a hostile test; benchmarks never regress.
- **Commits** (Q37–Q40): an undo journal under the filesystem host; see README.
- **Out of scope for now:** one-shot runs of agent-written code, dashboards, static signing keys. Clustering is no longer out of scope: it is prepared for in Roadmap 8, after the proxy.

## Phase 2: the Linux jail (done)

```
client ─► proxy ─► supervisor (nanovm serve): no untrusted code
                    │  HTTP/1 front end (front_threads), routes /{app}/... to the app's worker
                    │  per-app egress policy + public-IP filter + connect (the only network path)
                    │  spawns and restarts workers, places each in its cgroup
                    ├── fd 3: links for requests ──► worker "a"  (nanovm worker)
                    └── fd 4: links for egress  ◄── worker "a"  outbound requests
                                                          one Node, one app; jailed:
                                                          own uid · Landlock · seccomp · cgroup
```

- **Worker = one Node for one app.** The supervisor passes the app's config (JSON) and the component bytes on the worker's stdin. Later it passes bytes it has fetched and verified (Q24). The worker reads them, jails itself, and only then compiles and serves. Untrusted code never runs before the jail is in place.
- **Jail (Linux), applied by the worker to itself before any thread starts:**
  - **uid:** when the supervisor runs as root, each app gets its own uid (`jail_uid_base` + app index). The supervisor chowns the app's directories to that uid.
  - **Landlock:** read-write access to the app's `objects/`, `tmp/` and `cache/` subtrees; nothing else.
  - **seccomp:** an allowlist of the syscalls Wasmtime, tokio and the filesystem host need. There is no `socket`, `connect`, `bind`, `execve`, `ptrace`, `mount` or namespace calls, `clone` only creates threads, and anything else fails with `EPERM`. With no way to make a socket, the worker has no network beyond the two it inherited.
  - **no_new_privs** and `PR_SET_PDEATHSIG`, so a worker dies with its supervisor.
- **Mount and network namespaces were dropped from the Q23 plan.** Landlock already confines the filesystem, and seccomp's lack of `socket` already removes the network, so namespaces would be redundant defence.
- **cgroup v2 (supervisor):** when `cgroup` is set, each worker goes into `<cgroup>/<app>` before it is handed anything. `memory.max`, `cpu.max` and `pids.max` come from the app's `[app.worker]` section.
- **Links (`link.rs`), not HTTP/2.** Each direction is HTTP/1.1, one request per connection at a time, over socketpairs. The dialing side passes one end over a control socket with `SCM_RIGHTS` and pools the connections; a request runs inline in its own task at both ends.
  - The first version used one HTTP/2 connection per worker. It was slower, because every request crossed a shared connection task in each process. It also died after 1,024 stream resets (see README).
  - Each front-end thread has its own single-threaded runtime, `SO_REUSEPORT` listener and pool, so a request stays on one thread.
- **Egress:** in a worker, `EgressHooks` forwards each outbound request to the supervisor over its egress link. The per-object connection semaphore stays in the worker. The supervisor applies the allowlist, DNS and IP filter, then connects. A denial comes back as the same `ErrorCode` the guest saw before.
- **Failure:** a worker that dies takes down only its app. In-flight requests get `502 worker-down`, and the next request restarts the worker. A worker that fails to start isn't retried for 1 s. If the epoch thread ever dies, the worker aborts rather than run guests without CPU limits.
- **Observability:** OTLP exports wide request spans, correlated logs and worker/supervisor snapshot events (Q62). The former stats/events APIs and event ring were removed by W-2; readiness is an independent private 204 response.
- **In-process `Node`** stays the unit of the tests and benchmarks, and is what each worker runs. On macOS the same supervisor/worker topology runs without the Linux jail calls.
- **Tests:** the hostile guest run through the supervisor. A jailed worker cannot open a socket, read another app's or the supervisor's files, or exec; egress policy is enforced by the supervisor; a killed worker is restarted.
- **Cost (4-core sandbox, over HTTP with curl on the same cores):** about 0.1 ms more per request for one client, and about 25% less throughput at 64 in flight. In-process numbers are unchanged. README has the table.

## Phase 3: signed OCI deploys (done)

As in the target design, with these choices (README, "Apps from a registry", has the details):
- Headers `nanovm-app` (ref@digest), `nanovm-trust` (`<issuer> <subject>`, comma-separated) and `nanovm-min-version`. The version is the manifest's `org.opencontainers.image.version`.
- Verification happens before any blob is fetched. Bundles are fetched every time a worker starts (they are small, and whether one exists is the registry's current answer). Manifests and blobs are cached by digest.
- One worker per app at a time. A version switch fetches and verifies the new version first, then drains the old worker, then starts the new one. That also makes fetches single-flight per app, so no separate single-flight map was needed.
- A registry app's egress entries are its own (the public-IP filter still applies). Its numeric limits are clamped to `registry_limits` (`limits` since phase 7).
- Local `[[app]]` entries stayed, for development, the demo and tests (Q27), until phase 7 removed them.
- CI (`publish-guests.yml`) publishes the test guests with GitHub artifact attestations. That needed the repo to be public: attestations aren't available on private user repos.

## Phase 4: key-value and config (done), phase 5: WebSockets (done)

README's "Key-value and config" and "WebSockets" describe them.

## Phase 6: per-request commits (done)

A request's changes to `/data` (files and key-value) and its WebSocket publishes take effect only if it answers, durably. A failed request is rolled back. README's "Commits" describes the design and the alternatives rejected.

## Phase 6b: calls between objects (done)

An object calls another object of its app with an ordinary outgoing request to `http://nanovm/objects/{id}/{path}`, dispatched into the callee's queue (`egress.rs` `call`). A call that would wait on its own caller, directly or through other calls, is refused with 508; `Node::wait_for` checks and records the wait-for edges under the table lock. README's "Calls between objects" has the details.

## Phase 7: the front proxy (done)

Built on branch `proxy` by one agent and finished by another. README's "The proxy" and "Apps" have the details.
- **`nanovm proxy`** (`proxy.rs`) is the public entry point. It finds a host's app in the TXT record `_nanovm.<host>` (`app=<repository>:<tag>; trust=<issuer> <subject>; …; min=<version>`), asked of a DNS-over-HTTPS resolver and cached for its TTL; resolves the tag to a digest (cached for `tag_ttl_s`); drops every `nanovm-*` header a client sent; sets `nanovm-app`, `nanovm-trust`, `nanovm-min-version` and `nanovm-host`; and forwards to the node, WebSocket upgrades included. It speaks plain HTTP behind a TLS terminator (TLS was dropped, W-14). Whoever controls a domain controls its routing, but the node runs only what a listed identity signed, and an app is keyed by the name that owns its record (after CNAMEs) and its repository, so a record can't reach another name's app.
- **Apps come only from registries; one app per worker.** `[[app]]`, local components and the read-only `/app` directory are gone. A request's path is `/{object}/{rest}`, checked strictly (`table::route`).
- **App identity:** an app's name on the node, its volumes and its uid come from its host and `registry/repository` (`registry::app_name`: a readable prefix and a hash of the whole). Keyed by repository alone, as first built, any domain could switch another's app to an old signed version on its data; the review caught it.
- **Dev mode** (`dev = true`, refused unless `listen` is loopback): no signature check; registries and egress may be on private addresses; loopback registries are plain HTTP. Tests and the demo use it with a `container-registry` instance on loopback (`examples/dev-registry.rs`).
- **A registry client of our own** (`registry::Oci`, over reqwest) replaced oci-client's downloads: every download has a size cap, digests are checked before they name a file, and only public addresses are reached (DNS answers and redirects included) unless in dev mode. The runtime now depends on `oci-spec` for reference parsing and reads the few manifest fields it needs itself; oci-client is a dev-dependency (tests, `examples/dev-registry.rs`).
- **Review findings fixed here:** N-1, N-3, N-4, N-11, N-12, N-13 (and with it N-14, N-15), N-16, N-17, N-18, N-19. An adversarial review of the branch then found eleven more; the blocking ones were fixed before it landed (apps keyed by host, no unsigned manifests on disk, canonical registry addresses, proxy dev mode on loopback only, a bounded token and signature count), and the rest became N-22 to N-31 in ISSUES.md, fixed since: `max_apps` with idle apps' slots reused and registry fetches four at once with a deadline (N-22), accept loops that outlast fd exhaustion and proxy client timeouts (N-24), bounded single-flight proxy caches (N-25), bounded policy sets (N-26), a node-wide link cap (N-27), drains that wait for the old process (N-28), directory setup off the front end (N-29), hop-by-hop headers dropped (N-30), and oci-spec in place of oci-client (N-31, partly). Since then: dev mode needs the `dev` cargo feature (N-23) and the proxy has no TLS of its own (W-14).

## Runtime simplification analysis (2026-11-03)

**2026-11-06 update (Q86):** the current baseline is 5,082 runtime lines,
not the historical 5,305 below. Both unique pending state and separate per-phase
event-loop candidates passed tests but regressed measured throughput/churn and
were rejected. The original actor remains.
Worker termination uses one watch channel; dependency calls share tracing
and cancellation. Both commit gates and idle event-loop execution remain.
The retained slice is **5,077 lines**, plus the unchanged 242-line KV provider;
the under-5k goal is not yet met. Native-KV splitter removal below is obsolete
after provider composition; do not count it twice.

Prioritize precise local guarantees and recoverability: resolver deadlines,
local volume ownership, total persistent-data admission and quiescent
export/restore before automatic failover. The local ownership slice uses standard
nonblocking directory locks in App admission and supervisor preparation; it is
cooperative exclusion, not a lease or atomic handoff between processes. Memory may cache or stage work, but
does not replace durable commits. Multi-node operation still needs authenticated
ingress/internal-call placement, committed-state transfer, an acknowledgement/RPO
policy and storage-enforced writer fencing. The undo journal is not a replication
feed, and proxy-owned sockets still need cross-proxy publish routing.

The whole-runtime review starts at **5,305 lines**. The owner rejected the initial
100–200-line estimate as an overly incremental analysis, not a demonstrated ceiling.
The architectural reassessment identifies three ownership changes: the object task
owns each entire request lifecycle; storage owns completed mutations rather than
making KV/WASI coordinate transactions; and one dependency operation owns tracing,
admission and response-body lifetime. These designs remain uncompiled proposals.
Storage is 1,229 lines (23.2%); actor and
admission code is 894 (16.9%). The established counter includes 36 test-only lines
before a test module; moving those is not runtime simplification.

Start with actor ownership, then dependency calls and storage-owned mutations.
The smaller substitutions below are supporting opportunities, not the scope of
the redesign. Implement in separately verified slices:

1. **Worker lifetime (`supervisor.rs`, estimated 10–20 lines):** replace the
   oneshot kill plus watch completion channels with one watch channel. Its
   receiver must outlive child termination/reaping. Test natural exits, cancelled
   switches and a gated reap barrier; preserve the control runtime and slot transfer.
2. **Dependency calls (`egress.rs`, 25–50):** share span/cancellation, admission and
   body-permit ownership around route-specific work. Preserve cycle rejection
   precedence, external versus internal error trust, context propagation, counters
   and permit lifetime. No generic operation framework.
3. **Actor request ownership (`object.rs`, 45–90):** explore locally owned request
   state instead of two queue loops, shared optional `Pending` and store reset
   signalling. Keep deadline accounting unchanged initially. Wasmtime spawned
   work only progresses while the event loop runs; a plain idle `recv` would be a
   behavior change. Add private-fixture background-work coverage and measure warm
   throughput/churn/memory. Stop if preserving semantics recreates the machinery.
4. **Library substitutions:** Wasmtime `trappable_error_type` for KV's nested
   error splitter (10–20), and `http_body_util::Limited` for response byte counting
   (8–18). Preserve guest-error versus trap classification, streaming and aborts;
   retain only real net reductions after conversion code is included.
5. **Conditional storage experiment (20–60):** let the storage owner implement
   concrete KV file replacement and share actual admission/removal accounting.
   Do not merge quota, undo-log and retained-file budgets, weaken capability
   checks, or remove recovery records. Reject a generic transaction abstraction
   that merely relocates the sequence. Run all recovery-cut, rename, open-unlinked
   descriptor, repeated-replacement and post-return failure tests.

**Selective-WASI experiment (not adopted):** compiled and instantiated stock
filesystem bindings with ten guarded-method overrides on `bc92343`. Runtime
5,305 → 5,282 (−23); 105 unit/67 integration passed, one ignored; after tracing
field adjustment, 16 quota tests and all 67 integration tests passed again.
Production check passed; no benchmark. It removes forwarding but adds handwritten
registration/tracing macros and an explicit versioned override list. The new
registration test checks names exist, not that each stock method was intercepted;
TRACE targets also change. This is feasibility evidence, not a clear maintenance
win. Prototype saved outside tracked source and runtime changes restored.
The owner correctly distinguishes this implementation's weaknesses from the
underlying idea: selective registration remains worth exploring with safer
registration and behavioral interception coverage; it is not rejected in principle.

Incoming SQLite main `491e790` includes W-18: composed Wasm KV/config providers.
KV over filesystem imports could eliminate native KV mutation coordination rather
than centralize it in a new host API. Evaluate that experiment before committing
to the host-storage redesign below. W-18 is distinct from W-9 replication. The
selective-WASI results predate the SQLite integration and do not validate it.

**W-18 migration (Q75, in progress):** owner authorizes migrating guests/tests,
removing native KV and obsolete experiments, and revisiting the filesystem itself
where measurements expose problems. `components/keyvalue` supplies unchanged
Spin async CRUD and streaming WIT over `/data`; env/config remain injected.
The earlier sync KV/P3 probe trapped when suspension was required (Q73); it is
removed rather than retained as a compatibility implementation.

Mutations defer cancellation through temporary-file publication/cleanup; listing
slots last through completion delivery. Executed regressions caught cancellation
orphans, temporary-name collisions, and prematurely freed listing slots. Preserve
raw-file visibility, old descriptors and both host commit gates. The provider is
not a full Spin deployment implementation: valid labels address private object
directories, not manifest-selected shared stores. See
[the provider contract](components/keyvalue/README.md).

Integration/SQL/crash tests, byte/file/journal pressure and 10,000-key scale pass.
Review found pool exhaustion below configured object capacity; totals now match
per-component allowances. Full-quota reopen is also fixed. Matched release KV
performance regresses: 1 MiB GET 1.413 → 3.155 ms, PUT 4.614 → 7.126 ms.
Actual host+provider runtime is 5,300 → 5,287 (13 lines saved), with additional
build/CI wiring. See the provider README for all measurements and limitations.
The performance acceptance gate remains unmet. Profiling isolated a generated
Wasmtime byte-list copy loop: a diagnostic bulk-copy patch reduces 1 MiB GET
3.149 → 1.600 ms versus native 1.404 ms, without changing guest WIT or storage.
Sibling temporary files now avoid extra directory syncs; small-call and remaining
write costs still need attention. The dependency patch is not enabled. Publishing migrated guests on main remains a separate
approval from safe branch milestone pushes.

**Bounded backend screen (Q78):** [experiment and measurements](experiments/fs-backends/README.md)
compare the real journal with redb COW pages and SHA-256-addressed pages. Plain
pages reduce small-mutation/batch latency; hashing pays for repeated cross-file
content but slows unique large writes. Both slow warm reads and have fixed
per-store overhead. Fewer sync round trips can coexist with more bytes written.
No production replacement is selected. A next bounded proof should establish
inode/held-descriptor behavior, quota and per-object cache budgets before a WASI
adapter and mixed SQL/KV/raw-file/end-to-end comparison. This supersedes any
reading of the earlier storage plan as requiring the undo mechanism itself.
For external durability, keep accepted-version ordering, data retention until
replica acknowledgement, acknowledgement policy and writer fencing explicit.
Content hashes only identify data; the prototype's immediate reference reclamation
does not supply replica retention or an export protocol. Avoid Wasmtime patches
unless a demonstrated bug/inefficiency warrants separate upstream work.

**Inode proof (Q79):** both candidates now share a flat name-to-inode layer, held
descriptor ownership, logical quotas and orphan reclamation. Namespace and data
use one redb transaction; rejection invalidates all descriptors, while acceptance
preserves them. Owner preference is hash-addressed pages when costs are close,
not a blanket selection of plain pages. Range iteration and avoiding old-page
reads on full-page writes bring reads/small writes close at a 1 MiB cache; unique
large replacements still favor plain pages. Cache/RSS measurements, actual
namespace-scan complexity and physical amplification must inform integration.
This proof does not include WASI directories/streams, mixed SQL workloads, storage
fault recovery or replica export; keep the production host unchanged.

**Improvement execution:** `journal-runtime-improvements` keeps the journal baseline
and first reduces runtime 5,074 → 5,021 with behavior-preserving simplifications
borrowed from the other branch. Keep performance/lifetime changes separate and
measure against saved identical-guest controls. Do not import the registry's
separate awaited write/rename cleanup: cancellation semantics differ.

**Follow-through (Q85):** implemented bounded mailbox/actor storage, cached root
resolution, empty scratch reuse, request deadline reset and reactor-local links.
Lazy composed-KV buckets make fresh read-only requests 3.7× faster without
spending file quota. Warm runtime throughput improves about 15%, idle memory
143 → 119 KiB/object; durable writes remain essentially unchanged. Added
supervisor ancestor-directory sync and obsolete-path ENOTDIR handling. Final
runtime 5,082 versus 5,074: the overall LOC goal remains unmet, despite the initial
53-line cleanup. Retain journal; do not present this as the proposed large
filesystem simplification. Full suites, opt-in SQL and mutation evidence are in
HANDOFF. The owner approved main integration and migrated guest publication on
2026-11-06; older publication holds are superseded.

**Decision (Q84):** retain journaling. Optimized plain pages also received a real
matched comparison: small writes improve about 3× and unique 1 MiB writes reach
parity, but cold/recovery/churn remain 3–5× slower and replacement runtime grows
294 lines. This rejects the present per-object redb replacement, not pages in
principle. Continue the source-supported journal/runtime improvements from the
other branch with independent controls. [Decision and reproduction](experiments/fs-backends/README.md#adoption-decision-retain-journaling-2026-11-05).

**Historical integration gate (Q80):** `hashed-fs-integration` owns the next implementation.
Whole-namespace orphan scans are removed (Q81), and real-file I/O failure matrices
cover mutations and orphan recovery with shared content. A real component now proves
stock-WASI resource mapping and guest destruction without patches. Fixed file-length
ceilings pass exhaustion/recovery/locking tests under the unchanged cap (Q82).
They are not allocated-block or RSS caps, nor a guarantee of full-quota availability.
The hierarchical inode core and real WASI adapter are integrated behind temporary
`page-fs` (Q83), not selected by default. Actual runtime comparison finds faster
writes but worse cold starts, recovery, density and total replacement LOC. These
are failed adoption gates, not costs to hide with storage-only benchmarks.
Preserve directory confinement, descriptor identity, streaming bounds,
ephemeral `/tmp` with shared quotas, and both host commit boundaries. Compare
mixed KV/SQL/raw-file workloads through real jailed workers before switching.
Count all replacement runtime and provider code, not only deleted journal lines;
report latency, CPU, live-object memory, physical storage and ownership complexity.
The review's missing database-name sync is fixed with real syscall-error coverage;
shutdown now waits for stream/database destruction and the actual capped adapter
retains exclusive locks in the jail. Real reader/writer/directory streams now pass
descriptor-drop, acceptance, quota-reclamation and rejection/restart coverage;
premature inode release fails the regression, and restored code passes. Finish
persistent-cap/RSS amplification, ancestor-directory durability and backend-neutral
optional SQLite oracles. Investigate eager database creation/reopening before
accepting cold/churn regressions. [Current evidence and successor prompt](experiments/fs-backends/README.md#runtime-integration-checkpoint-2026-11-05).

The separate improvement branch's source assessment is in HANDOFF (2026-11-04).
Reuse its benchmark instrumentation and lazy-bucket idea, but measure any provider,
scheduler or dependency change on both controls. Its SDK changes the guest's write
strategy, so importing its guests would invalidate a storage-only comparison.
Root-descriptor lookup caching is an optimized journal control; page directories
should use stable IDs instead. Defer custom wakers, socket I/O wrappers and unrelated
features from this slice. Security fixes need separate validation, not backend credit.

For actor ownership, use sequential `run_concurrent` phases with timeouts outside
each invocation, retaining event-loop driving while idle. For storage, expose
complete `replace_value`/`delete_value`/`resize`/`rename` operations and represent
staged rename outcomes explicitly, while keeping journal format/recovery intact.
These changes remove second owners of operations, not just helper boilerplate.

Rejected as unproven or product changes: one universal HTTP client/cache, replacing
the journal with SQLite, removing meaningful spans, and freezing idle guest work.
Keep the performance-sensitive socketpair links and pure OCI parse cache. Do not
inflate application code with library-style builders or traits.

No new product decision blocks this sequence. Full analysis and evidence:
[runtime simplification review](https://ampcode.com/artifacts/nanovm-runtime-simplification-analysis-and-test-reliability-d937c6ea-f380-4eda-8719-307f9c4b875a).
Use the workspace `writing-rust` and `grilling` skills. Every implemented slice
needs targeted tests/mutations, full suites, demo and matched performance checks;
keep proposals distinct from delivered savings.

## Roadmap (agreed with the owner, 2026-10-30)

Each item has an entry in [ISSUES.md](ISSUES.md) (`W-n`) where its status is tracked; the decisions are Q41–Q51 in [HANDOFF.md](HANDOFF.md). In priority order:

1. **WebSockets moved into the front proxy (Q42, Q53; W-3 done).** The proxy owns sockets and app/object-scoped channels; the node answers ordinary event POSTs. Each event resolves the current digest, so sockets and subscriptions survive worker exits and deployments. A private publish listener receives committed publishes, stamped with the app by the supervisor and object by the worker. Socket caps belong to the proxy, shared directly with socket managers; an object-keyed tree bounds fan-out work to that object's sockets (Q56). This supersedes Q31/Q35; full-suite, mutation, demo and benchmark results are in HANDOFF. Review follow-ups N-33/N-34 remain in ISSUES.
2. **Observability by default (Q43, Q62, Q71; W-2 complete).** Always-on rich request-completion spans carry queue, instantiate, guest, commit and post-return timing fields; real object calls retain child spans. Guest logs correlate with their request. Infrastructure snapshots are structured OTLP logs, replacing metric instruments and local histograms. No tracing feature or compatibility endpoints remain. Use the OpenTelemetry SDK and configurable Collector, not a custom event framework.
   - Delivered on main in `49d0ae4`; main and branch CI passed. Recommend a separately budgeted standalone Collector (or dedicated physical cores), private ingress and TLS/access controls across hosts. Measured application overhead is 2.17%, versus 7.58% fully colocated; Collector CPU is additional. Remote-network and durable-backend behavior still require deployment-specific validation. See README's operator checks for configuration and alerts.
   - Simplification follow-up: existing scope guards replace a custom wait-edge cleanup type; DNS results are checked without a second collection. Runtime 5,315 → 5,305. The reproduced registry drain test defect is fixed by testing persistence with a tiny file/key, waiting for observed handling, and checking worker lifetime instead of client scheduling order. No production budget or guest changes. Deep analysis above replaces the earlier narrow cleanup plan.

   <details>
   <summary>Historical implementation and validation checkpoints (not current blockers)</summary>

   - Deployment conclusion (Q71): separately budget the standalone/gateway Collector, or dedicate physical cores on the same host. Owner delegates this choice to engineering; measured 2.17% matched application overhead satisfies that deployment's gate. Colocated overhead remains 7.58%, with real additional Collector CPU. Runtime 5,315 versus main baseline 4,955 (+360, 7.3%); no arbitrary LOC refactor. Same-host Collector validation is complete, not remote-machine or durable-backend validation. Earlier pending-decision checkpoints below are superseded; all correctness/verification gates passed and fast-forward delivery is authorized.
   - Post-commit acceptance: equal application physical-core allocation gives main/W-2 18,631/18,227 requests/s (2.17% lower), while fully colocated is 27,548/25,459 (7.58% lower). Active application CPU 103.45/105.00 µs/request isolated and 109.42/110.64 colocated; Collector adds 14.70/13.62. All exports complete, including 4m spans during 2m sustained requests. Full locked suite twice, production check, demo, quick benchmark and mutations pass. Preserve optimization; do not merge pending owner decision about separately budgeting Collector CPU. This is not a claim of few-percent whole-system overhead or durable-backend validation. HANDOFF contains reproduction steps and prior results.
   - Final optimization (Q70): owner retains all meaningful spans; successful supervisor spans must not be consolidated. Native profiling found repeated OCI parsing; cache only pure parse results per thread (16 owned references, input ≤1024 bytes), with all trust/host/version/digest checks per request. Reserve common span attributes. Runtime 5,315 lines. Matched three-pair HTTP controls show 0.74% lower aggregate throughput with Collector physically isolated and 4.45% lower colocated, all 2.4m spans per matrix delivered. Cache mutations/full suite/production check/demo pass. Post-commit repeat and delivery remain; older measurements below describe earlier checkpoints, not current product decisions.
   - Correctness before optimization (Q68): instrument meaningful work, causal relationships and outcomes using Honeycomb's wide-event model and standard OpenTelemetry. Requests are SERVER spans; object and external HTTP calls are CLIENT spans with target/status/error context. External calls inject W3C context using the standard propagator; spans and denial logs omit queries. Finish pre-worker failures and background attribution coverage before throughput tuning. Sampling is an explicit trace policy, never accidental queue loss.
   - Q69 records cancelled dependencies, ignores untrusted external error headers and correlates runtime diagnostics using explicit context. N-37 now records final-commit failures without changing the sent status; a private test component verifies actual post-return call/log context. Proxy/supervisor SERVER events cover pre-worker failures and parent worker events, ending at headers. Select one layer for derived request counts. Full locked suite passes (104 unit, 67 integration, one ignored); performance and review follow-through remain.
   - The owner suspended the hard LOC budget; CI reports size (currently 5,315) without enforcing 5,000. Guest and infrastructure logs use one bounded emission path. Preserve bounded Collector responses for the SDK's partial-success parser; SDK diagnostics go to stderr, not recursively into OTLP.
   - The supervisor stamps app/digest identity; no jail permission changed. Readiness is independent of telemetry. Request analytics and sampling belong downstream; bounded best-effort export is not exact accounting.
   - Q66 retains Q64's 256-span batches, 1 MiB body cap and stamping at the worker trust boundary, with a 2048-span bounded queue and jemalloc in the Linux CLI/benchmark, not the library. The reproducible `bench --export` receives all 601,000 request spans at 75–79k/s in three invocations, including sink decoding on the same cores. Retained memory increases; production Collector/jailed-worker capacity and matched main comparisons remain open. Favor library choices over custom export machinery.
   - Real HTTP validation uses off-the-shelf oha and the official OpenTelemetry Collector, not a custom HTTP load generator. N-36 records substantial worker export-queue loss despite successful HTTP requests. Reject the experimental concurrent SDK processor until its flush barrier is correct (Q67).
   - N-36's reproduced scheduling loss is fixed by a dedicated supervisor control runtime and captured startup Handle. Relay/reaper/export I/O no longer shares the first frontend's reactor. Mean worker export roundtrip fell from 12.6 to 3.5 ms; a sustained 2m HTTP request run delivered all 4m spans to the official Collector with derived metrics enabled at 25.3k requests/s. The runtime-lifetime mutation is caught. No SDK batching, queue, wire protocol, sampling or jail change. Ordinary outgoing calls already run concurrently; only each SDK exporter serializes its own batches.
   - The remembered ~64k main result is reproduced using the same in-process workload on historical/current main. W-2 with a decoding sink is 75–78k/s with complete export; these are not HTTP rates. Historical main itself is 27–29k/s over real HTTP on this orb. Do not equate the old result with a production baseline or claim the requested 64k HTTP target is met.
   - Final alternating clean-build HTTP controls measured current main 23.8–25.2k/s versus W-2 17.2–23.3k/s, all spans delivered. Preserve this contradictory result alongside the earlier 26k/s and sustained 25.3k/s observations: the HTTP performance gate is not cleared. An ambiguous shared-target Cargo control was discarded and main rebuilt; HANDOFF records exact provenance.
   - Post-commit controls on `1adadcc` isolate physical SMT core pairs, with the application on CPUs 0–1 and Collector/oha on 2–3. Across six warm 200k-request runs per variant, aggregate main is 13,001/s versus W-2 11,635/s (10.5% lower), with all 2.4m expected spans delivered. Application CPU is 147.0 versus 165.2 µs/request, excluding the Collector's additional 23.45 µs/request. The gap is not solely Collector contention. Acceptance is within a few percent of matched main; the historical 64k in-process result is not a new HTTP target. A diagnostic removal of supervisor-hop spans improves 2.6–8.7% but also removes coverage; it is not retained. Consolidating successful hop events would change independent node-ingress latency visibility and must be decided explicitly, preserving failures/startup instrumentation. Batch1024 has inconsistent gains and oversized-payload risk. No sampling or experimental exporter is adopted; W-2 remains open.
   - `demo/otel-collector.yaml` pins a validated official contrib 0.164.0 reference: memory limiter, batching, span-derived bounded request/error/duration metrics, no sampling, Prometheus and debug exporters. Counts and histogram totals match 148 successful reads/writes and 17 traps per layer. Operators must select one span layer and replace debug with authenticated durable backend export; README explains bounds, alerts, overflow and backend validation.
   - Tests and demo consume decoded OTLP. SDK shutdown stays off the async runtime thread. Collector responses and SDK rejection diagnostics are tested through HTTP and worker links; further hostile-load/trace-parentage coverage, review and the performance gate remain before landing. HANDOFF records historical measurements and the current event-model validation; fewer spans alone must not be claimed as a throughput win.
   </details>

3. **A per-object SQLite demo (Q46).** The showcase: every object its own SQLite database on `/data`, committed with its response. It works today, since writes happen in place and `mmap` is kept. (W-1)
4. **Alarms, outside the node (Q41).** A scheduler beside the proxy sends a request to an object when its alarm fires; an object sets alarms with a response header. The node stays request-only. (W-4)
5. **Data lifecycle (Q44).** Planning first: deleting an app's or an object's data, exporting a volume, backups and retention. (W-5)
   - **Offline recovery slice (Q88):** operator-only `scripts/backup.py` uses restic, not a custom archive or runtime endpoint. Stop every writer; hold all existing app-directory locks while copying the complete objects tree, including undo logs and retained files. Tag only complete snapshots, restore an exact ID to a nonexistent root, verify contents and sync before serving. Deployment inputs remain separately retained. The real-process drill removes access to the original root and checks SQL/file/KV crash rollback plus new writes across restart. Runtime remains 5,085 lines; tooling is counted separately, not a runtime reduction. Live snapshots, deletion authorization, automated retention, off-node deployment validation and distributed failover remain future work.
   - **Validated at `fab908c`:** real-process drill passes as root and unprivileged; all default and optional SQL tests, CLI failure/ownership tests, six mutations, demo and quick benchmark pass. Review's credential-path problem is fixed without per-backend environment normalization. Runtime and the benchmark binary are unchanged. Restic is installed by CI and idempotent orb setup; no remote repository or production cutover was provisioned.
6. **More guest languages and a publish command (Q47).** JS and Python guests through componentize-js and componentize-py; `nanovm publish` wrapping the `oras` push and keyless signing. (W-6, W-7)
7. **Numbers against the alternatives (Q47).** Density, cold starts and warm latency against workerd and Spin. (W-8)
8. **Toward many nodes (Q48–Q50).** Research and groundwork, after the proxy lands:
   - **Committed versions shipped off the node:** separate three concerns: coherent tentative state that can be rejected, local commit, and acknowledgement after the selected durability policy. Undo logging is today's implementation, not a requirement; snapshots, copy-on-write or transactional storage are valid candidates. Replication needs ordered committed content/namespace changes or immutable snapshot manifests, not merely the current undo log. Capture the committed version before subsequent writes can alter it. Include object identity, sequence and fencing epoch; replay must be idempotent. Decide whether success means local durability, remote backup or a regional quorum before promising failover guarantees. Async replication implies a nonzero loss window. (W-9)
   - **Placement and leases:** the proxy places each object on a home node (rendezvous hashing to start); a node takes a lease with an epoch before instantiating an object, and every commit carries the epoch, so a node that lost its lease can't commit. On one node the lease is always granted, so the hook can land early. (W-10)
   - **Requests that don't write can run in parallel** (Q50): stateless routes on any node, and read-only requests on read replicas of an object fed by committed change records. On one node it needs care: the undo journal writes in place, so a concurrent reader would see uncommitted data. (W-11)
9. **Cleanups:** the rest of the review's removable-code list (registry cache and re-hash, config memory checks), for the agent doing the rewrite. (W-13)

## Follow-ups

- gVisor-systrap comparison.
- Quota on the WASI 0.2 filesystem (shadowing p2 writes).
- Wizer pre-initialisation (not published for 50.0.0, and it needs a custom init export).
- A trusted caller header on calls between objects, so an object can tell a call from a client request. (W-12)

## Risks

- P3 in wasmtime 50 is experimental, so API rough edges are likely; workarounds stay local to `object.rs` and `quota.rs`.
- `poll_no_interesting_tasks` ignores spawned guest threads, so `post_return_timeout` is the backstop.
- How much virtual address space macOS allows limits density with 4 GiB slots; the benchmark will report it.
