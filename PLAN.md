# nanovm: plan for the portable core

## Context

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
- `demo/demo.sh` drives thousands of objects and the attack storm over HTTP. The node's only control surface is two JSON APIs, `/_nanovm/stats` and `/_nanovm/events` (deprecated in favour of OpenTelemetry; see Roadmap).

## Architecture

```
client ──HTTP──► nanovm (hyper 1, http1)
                  │  data:    /{object}/{rest...}, app named by headers (guest sees /{rest...})
                  │  control: /_nanovm/stats, /_nanovm/events (JSON)
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
    src/metrics.rs               atomic counters, log2 latency histograms, event ring buffer, RSS
    src/server.rs                hyper front end, routing, /_nanovm/{stats,events}
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

- **Placement:** the node sits behind a trusted proxy on a private network. The proxy is the public HTTP(S) entry point (TLS, auth, route → digest); only the proxy-to-node hop is private. The node has no TLS and no auth of its own.
- **Tenancy:** tenant = app, keyed by its host and repository (phase 7). Routes are `/{object}/{path}`, with the app named by the proxy's headers. The node exposes only data APIs.
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
    - The supervisor itself speaks the protocol and handles channel publishing; there is no Pushpin process (Pushpin can't be embedded as a library).
    - Events are ordered per object. Channels are scoped per object, as in Durable Objects (Q32).
    - Sockets close when their worker goes, including on a version switch (Q35).
    - Bounds come from the existing request and response limits, plus `max_sockets` per object and per node (Q36). tric's sizes were API Gateway's, so they weren't carried over.
  - **Alarms** (Q30, then Q41): skipped so far; queued as a scheduler outside the node (Roadmap 4).
  - **Key-value** (done): a host-implemented `wasi:keyvalue` stored as files under `/data`. `wasi:filesystem` stays exposed. `wasi:config` serves each app's config values.
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
- **Data APIs:**
  - `/_nanovm/stats` returns the supervisor's view plus each worker's stats, keyed by app.
  - `/_nanovm/events?app=<a>&since=` serves that worker's event log.
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
- **`nanovm proxy`** (`proxy.rs`) is the public entry point. It finds a host's app in the TXT record `_nanovm.<host>` (`app=<repository>:<tag>; trust=<issuer> <subject>; …; min=<version>`), asked of a DNS-over-HTTPS resolver and cached for its TTL; resolves the tag to a digest (cached for `tag_ttl_s`); drops every `nanovm-*` header a client sent; sets `nanovm-app`, `nanovm-trust`, `nanovm-min-version` and `nanovm-host`; and forwards to the node, WebSocket upgrades included. TLS is optional. Whoever controls a domain controls its routing, but the node runs only what a listed identity signed, and an app is keyed by its host and repository, so a record can't reach another host's app.
- **Apps come only from registries; one app per worker.** `[[app]]`, local components and the read-only `/app` directory are gone. A request's path is `/{object}/{rest}`, checked strictly (`table::route`).
- **App identity:** an app's name on the node, its volumes and its uid come from its host and `registry/repository` (`registry::app_name`: a readable prefix and a hash of the whole). Keyed by repository alone, as first built, any domain could switch another's app to an old signed version on its data; the review caught it.
- **Dev mode** (`dev = true`, refused unless `listen` is loopback): no signature check; registries and egress may be on private addresses; loopback registries are plain HTTP. Tests and the demo use it with a `container-registry` instance on loopback (`examples/dev-registry.rs`).
- **A registry client of our own** (`registry::Oci`, over reqwest) replaced oci-client's downloads: every download has a size cap, digests are checked before they name a file, and only public addresses are reached (DNS answers and redirects included) unless in dev mode. oci-client stays for reference parsing and manifest types.
- **Review findings fixed here:** N-1, N-3, N-4, N-11, N-12, N-13 (and with it N-14, N-15), N-16, N-17, N-18, N-19. An adversarial review of the branch then found eleven more; the blocking ones were fixed before it landed (apps keyed by host, no unsigned manifests on disk, canonical registry addresses, proxy dev mode on loopback only, a bounded token and signature count), and the rest are open as N-22 to N-31 in ISSUES.md. N-22 (unbounded apps per node) is fixed: `max_apps` with idle apps' slots reused, a bounded worker map, and registry fetches limited to four at once with a deadline.

## Roadmap (agreed with the owner, 2026-10-30)

Each item has an entry in [ISSUES.md](ISSUES.md) (`W-n`) where its status is tracked; the decisions are Q41–Q51 in [HANDOFF.md](HANDOFF.md). In priority order:

1. **WebSockets moved into the front proxy (Q42).** The proxy itself landed as phase 7. It then also holds client WebSockets, as Pushpin does: the node only ever answers requests, and an object sees socket events as plain `POST`s in the WebSocket-over-HTTP format it already gets. Most of `ws.rs` leaves the node, and N-14 and N-20 go with it. This supersedes Q31 (sockets in the supervisor). (W-3)
2. **Observability by default (Q43).** OpenTelemetry tracing, metrics and guest logs, on by default, tagged per tenant (app, object, version). `/_nanovm/stats` and `/_nanovm/events` are deprecated and go once OTel covers them. A jailed worker has no sockets, so its telemetry leaves through the supervisor. (W-2)
3. **A per-object SQLite demo (Q46).** The showcase: every object its own SQLite database on `/data`, committed with its response. It works today, since writes happen in place and `mmap` is kept. (W-1)
4. **Alarms, outside the node (Q41).** A scheduler beside the proxy sends a request to an object when its alarm fires; an object sets alarms with a response header. The node stays request-only. (W-4)
5. **Data lifecycle (Q44).** Planning first: deleting an app's or an object's data, exporting a volume, backups and retention. (W-5)
6. **More guest languages and a publish command (Q47).** JS and Python guests through componentize-js and componentize-py; `nanovm publish` wrapping the `oras` push and keyless signing. (W-6, W-7)
7. **Numbers against the alternatives (Q47).** Density, cold starts and warm latency against workerd and Spin. (W-8)
8. **Toward many nodes (Q48–Q50).** Research and groundwork, after the proxy lands:
   - **Change records shipped off the node** (Litestream-style): the journal already knows what each commit touched, so a commit can emit a self-contained record of it. Replicated, they make the node a cache of its volumes. (W-9)
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
