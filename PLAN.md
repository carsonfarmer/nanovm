# nanovm: plan for the portable core

## Context

The goal is to run large numbers of untrusted wasip3 HTTP components at high density. Each one must be strongly isolated with hard limits.

The run model is shaped like Durable Objects:
- Each `(app, object-id)` pair has exactly one live instance.
- Requests to that instance are served one at a time.
- In-memory state is lost when the instance is evicted.
- Each object has its own persistent disk.

Production targets Linux cloud VMs without KVM. This phase is the **portable core**, which runs natively on macOS and Linux. The Linux jail (seccomp, Landlock, cgroup v2, a process per tenant) comes later, behind a `Jail` trait.

The stack is wasmtime 50.0.0 with wasmtime-wasi 50 and wasmtime-wasi-http 50 (P3 support enabled by default). P3 is marked experimental upstream, with no semver guarantees.

Done means:
- `nanovm serve` hosts real guest components with every limit enforced.
- `cargo test` proves each guarantee against hostile guests.
- `examples/bench.rs` prints cold-start, warm-latency, throughput and density numbers.
- A live dashboard demo shows thousands of objects alongside attack scenarios.

## Architecture

```
client ──HTTP──► nanovm (hyper 1, http1)
                  │  data:    /{app}/{object}/{rest...}   (guest sees /{rest...})
                  │  control: /_nanovm/ (dashboard), /_nanovm/stats, /_nanovm/events
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
                  ├─ VFS (wasi:filesystem@0.3, quota'd):  /app RO · /tmp scratch · /data persistent
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
    src/quota.rs                 quota'd wasi:filesystem@0.3 host (QuotaFs), mount side map, QuotaWriter
    src/egress.rs                allowlist, IP classifier, resolver + connector, send_request
    src/metrics.rs               atomic counters, log2 latency histograms, event ring buffer, RSS
    src/server.rs                hyper front end, routing, control API, embedded dashboard, demo load tools
    src/main.rs                  `nanovm serve --config demo/nanovm.toml`
    examples/bench.rs            in-process benchmark driver (no sockets)
    tests/integration.rs         integration tests against real guest components
  guests/                        separate workspace, target wasm32-wasip2, cdylib + wasip3 0.9
    counter/                     benign DO: in-memory hits + persisted counter + kv in /data
    hostile/                     attack endpoints (see Tests)
    util/                        shared request/response and wasip3 filesystem helpers
  dashboard/index.html           live demo UI (embedded via include_str!)
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

- **Preopens:** three calls to `preopened_dir(host, guest, FsPerms)`:
  - `/app` read-only, shared per app (`apps/<app>/static`)
  - `/tmp` read-write, at `scratch/<app>/<object>-<instance>`
  - `/data` read-write, at `data/<app>/<object>`
- **Mount side map:** `rep → MountId`, filled in by the shadowed `preopens.get-directories` and `open-at`. Those are the only two places a Descriptor is created, and descriptors don't store paths.
- **`MountQuota`:** a mutex around `{bytes, files}` plus limits `{max_file_size, max_total_bytes, max_file_count}`. `/data` is walked at mount time; `/tmp` starts empty.
- **Writes:** `write-via-stream` and `append-via-stream` use our `QuotaWriter: StreamConsumer`, which writes inline: the quota check and `write_at` happen under the quota lock, writing as much as the budget allows, so a filler sees a short write and then an error, as on a full disk.
  - Going over the mount's byte quota returns `insufficient-space`.
  - Going over the per-file limit returns `file-too-large`.
  - Going over the entry limit returns `quota`.
  - When dropped before a result is sent, it sends `Ok(())`.
- **Pre-checked:** `open-at` with CREATE (entry count) or TRUNCATE (credits the old size), `create-directory-at`, `set-size`.
- **Credited back:** `unlink-file-at`, `remove-directory-at`, and `rename-at` when it replaces a *different* existing file. A rename onto the same file (compared by `metadata_hash_at`) is a POSIX no-op and credits nothing.
- **Denied:**
  - `link-at` and `symlink-at` on metered mounts, so shared inodes and link targets don't have to be tracked.
  - Cross-mount `rename-at` and `link-at` return `CrossDevice`.
  - Writes to `/app` return `ReadOnly`.
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

## Limits (per app, in each `[[app]]`'s `[app.limits]`)

| Knob | Default | Enforced by |
|---|---|---|
| `memory_mb` | 64 | custom ResourceLimiter (all memories) + pool `max_memory_size` |
| `cpu_ms` per request | 200 | wall time inside store polls, checked by the epoch callback |
| `request_timeout_ms` / `post_return_timeout_ms` / `idle_timeout_ms` | 5000 / 5000 / 30000 | outer deadline wrapper |
| `max_request_body_kb` / `max_response_body_kb` | 1024 / 8192 | body wrappers |
| `tmp_mb` / `data_mb` / `max_files` / `max_file_mb` | 16 / 64 / 1000 / 16 | MountQuota |
| `max_queue` | 64 | bounded mailbox |
| `max_outbound_concurrency` | 4 | egress semaphore |
| `max_resources` | 10000 | ResourceTable + component handles |
| `egress` | `[]` (deny all) | allowlist |

Node-level settings: `max_live_objects` (1000), `memory_reservation_mb` (128), `listen` (127.0.0.1:8080), `data_dir` (nanovm-data), `epoch_tick_ms` (5), `heavy_lanes` (cores that requests past 10 ms of CPU may occupy at once; 0 means half the cores), `demo` (enables the load-generation endpoints; must be off in production), `dashboard`.

## Guests (guests/, wasm32-wasip2 + wasip3 0.9 + `async-spawn`)

`rustc`'s bundled `wasm-component-ld` produces the component directly, with no adapter or wasm-tools needed.
- **counter** (benign object):
  - `GET /`: object id, in-memory hit count, persisted counter
  - `POST /incr`
  - `GET/PUT/DELETE /kv/{key}`, stored in `/data/kv`
  - `GET /static`: reads `/app`
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
- **Quota:** filling `/tmp` or `/data` returns `insufficient-space`, `/files` returns `quota`, `/fill` past the file cap returns `file-too-large`, `/write-app` is refused, `/escape` (`..`, absolute paths, symlinks) is denied, and `/cross-rename` returns `cross-device`. `/data` usage is rescanned after a restart. Renaming a file onto itself credits nothing, while a rename over another file frees it.
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

`nanovm serve --config demo/nanovm.toml`, then open `http://127.0.0.1:8080/_nanovm/` for a live dashboard. It shows:
- live objects, instantiations, evictions and poisonings
- requests per second, p50/p99, RSS
- an event feed

Buttons:
- "spawn 10,000 objects"
- "hammer one object"
- attacks: memory bomb, infinite loop, disk filler, SSRF to metadata, path escape, handle leak

A "neighbor" sparkline keeps pinging a benign object throughout to show it is unaffected.

`demo/demo.sh` runs the same scenario from the terminal with curl.

## Order of work (in this session)

All nine steps are done. [README.md](README.md) has the results and the bugs the tests and benchmarks found.

1. Guests workspace plus a minimal `counter`; confirm the toolchain and inspect the component imports.
2. Engine + linker + one object actor serving end to end over HTTP.
3. Limits, deadlines, poisoning and re-instantiation, with metrics.
4. Quota'd filesystem.
5. Egress.
6. ObjectTable with LRU/idle eviction, control API, dashboard.
7. Integration tests.
8. Benchmarks.
9. Demo script, dashboard check in the browser, and results.

## Out of scope for this phase / follow-ups

- Linux jail (seccomp, Landlock, cgroup v2) and a worker process per tenant.
- gVisor-systrap comparison.
- Quota on the WASI 0.2 filesystem (shadowing p2 writes).
- Wizer pre-initialisation (not published for 50.0.0, and it needs a custom init export).
- Multi-node placement and leases.
- A transactional key-value store per object.

## Risks

- P3 in wasmtime 50 is experimental, so API rough edges are likely; workarounds stay local to `object.rs` and `quota.rs`.
- `poll_no_interesting_tasks` ignores spawned guest threads, so `post_return_timeout` is the backstop.
- How much virtual address space macOS allows limits density with 4 GiB slots; the benchmark will report it.
