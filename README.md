# nanovm

A wasmtime-only host for untrusted WASIp3 HTTP components, run as Durable-Objects-style objects:

- Each `(app, object-id)` has exactly one live instance, which serves one request at a time.
- Each object has its own memory cap, CPU budget, deadlines and handle cap.
- Each object has its own quota'd filesystem: read-only `/app`, scratch `/tmp`, and a persistent `/data` volume.
- Each app has an egress allowlist.
- Each object has a `wasi:keyvalue` store in its volume, and each app its `wasi:config` values.
- Objects accept WebSockets and publish to their own sockets.
- Memory is lost on eviction; `/data` survives eviction and restarts.

Each app runs in its own jailed worker process. A supervisor that runs no guest code owns the network (see [The jail](#the-jail)). [PLAN.md](PLAN.md) has the design and what comes next.

## Try it

```bash
demo/demo.sh
```

The script:
1. Builds the guests and the node.
2. Starts a node from [demo/nanovm.toml](demo/nanovm.toml), with a benign `counter` app and a `hostile` app on the same host.
3. Pings a canary object throughout.
4. Drives it over HTTP with `curl --parallel`: per-object volumes, path escapes and egress, 5,000 cold starts, 50,000 warm requests, `/data` writes, and a storm of 110 simultaneous attacks.

It leaves the node running. Its data APIs are `GET /_nanovm/stats` (the supervisor's view plus each app worker's counters and latency histograms, as JSON) and `GET /_nanovm/events?app=A&since=N` (that app's kills, denials and evictions).

Requests are routed as `/{app}/{object}/{path}`:

```bash
curl -XPOST http://127.0.0.1:8080/counter/alice/incr
```

## What a run of `demo/demo.sh` shows

On the 4-core Linux sandbox, with each app's worker capped at 8,000 objects (the sandbox's fd limit; see Host requirements). These numbers go through curl, which shares the 4 cores, and the HTTP front end; the in-process numbers are under Benchmarks. The second column is the same demo on the unjailed single-process node from before the jail, on the same machine.

| Scenario | Jailed | Unjailed |
|---|---|---|
| 5,000 cold starts (64 in flight) | 3.7K/s, p50 14 ms, 89 KB RSS per new object | 4.0K/s, p50 14 ms, 87 KB |
| 50,000 GETs over 1,000 warm objects | 19K/s, p50 2.2 ms | 27K/s, p50 1.2 ms |
| 10,000 `POST /incr` (each one rewrites `/data/count`) | 6.7K/s | 7.3K/s |
| One object, one client | 3.0K/s, p50 0.3 ms | 5.6K/s, p50 0.1 ms |
| 110 attacks at once | All contained in 2.2 s; the 2 s wall-clock deadline sets the slowest | the same |
| Canary over HTTP, throughout | 0 failures | 0 failures |

Every request succeeded and each worker started once. The cost of the jail is the hop between the supervisor and the worker; see [What the jail costs](#what-the-jail-costs).

Every attack in the storm, and how it ended:

| Attack | Outcome |
|---|---|
| allocate 1 GiB | killed at the 32 MiB memory cap |
| spin forever | killed at the 100 ms CPU budget |
| sleep 60 s | killed at the 2 s deadline |
| panic | trap; instance discarded |
| leak 1M handles | killed at 2,000 handles |
| fill the disk | write refused at the 8 MiB disk budget |
| create files | refused at 200 files |
| write `/app` | refused (read-only) |
| `../` and symlink escapes | refused |
| fetch cloud metadata (169.254.169.254) | denied (not on the allowlist) |
| allowlisted name that resolves to 127.0.0.1 | denied by the resolved-IP check (in this sandbox, which has no DNS, it fails to resolve instead; the `localhost` integration test covers the IP check) |

## The jail

```
client ─► proxy ─► supervisor (nanovm serve): runs no guest code
                    │  HTTP/1.1 front end; routes /{app}/... to the app's worker
                    │  makes every outbound request: allowlist, DNS, public-IP filter
                    │  starts and restarts workers; places each in its cgroup
                    ├── fd 3: links for requests ──► worker "counter"  (nanovm worker)
                    └── fd 4: links for egress  ◄──  one Node for one app, jailed
```

A worker reads its config and component from stdin, then jails itself while it still has one thread, before it compiles or runs anything:

- **Its own uid.** When the supervisor runs as root, app `i` runs as `jail_uid_base + i`, and its directories are chowned to it.
- **Landlock.** It gets read-write access to the app's own `objects/`, `tmp/` and `cache/`, read-only access to its `static_dir`, and nothing else.
- **seccomp.** The allowlist holds only the syscalls Wasmtime, tokio and the filesystem host make, traced over the test suite:
  - no `socket`, `connect`, `bind`, `execve`, `ptrace`, `mount` or namespace calls
  - `clone` for threads only
  - no memory that is writable and executable at once
  - limits can be read, never raised

  Anything else fails with `ENOSYS`. With no way to create a socket, a worker's only channels are its two control sockets to the supervisor.
- **`no_new_privs`**, and the worker dies with its supervisor.
- **cgroup v2** (optional, `cgroup = "<delegated dir>"`). Each worker goes into `<cgroup>/<app>` with that app's `[app.worker]` memory, CPU and pids limits, before it is given anything.

Mount and network namespaces were left out on purpose: Landlock already confines the filesystem, and with no `socket()` there is no network to remove.

**Links.** The supervisor and a worker talk HTTP/1.1, one request at a time per connection, over socketpairs. The dialing side passes one end of each pair to the other over its control socket (`SCM_RIGHTS`) and keeps the connections it has made in a pool. A request runs inline in the task that sent it, at both ends: on this path every handoff between tasks is a thread wakeup, and those cost more than the HTTP framing. Each front-end thread (`front_threads`, a quarter of the cores by default) has its own runtime, listener and pool.

A worker that dies takes only its app with it. In-flight requests get `502 worker-down`, and the next request starts a fresh worker. Volumes survive.

### What the jail costs

The hop between the processes costs two extra cross-process round trips, about 30–40 µs each in this VM, plus one more HTTP parse. On the 4-core sandbox:

- One client sees about 0.1 ms more latency, the warm GET p50 above.
- At 64 in flight, throughput is about 25% lower. The supervisor needs about a fifth of a request's CPU, and here it shares 4 cores with the worker and with curl.

The in-process benchmark below measures the node inside a worker, and is unchanged.

## Apps from a registry

A local app comes from the node config (`[[app]]`, used by the demo and most tests). A registry app is named by the proxy, with each request:

```
nanovm-app: ghcr.io/org/app@sha256:…        # by digest only
nanovm-trust: <issuer> <subject>[, …]        # whose signature is accepted
nanovm-min-version: 1.4.0                    # optional floor
```

The node trusts these headers because only the proxy can reach it; the proxy must drop any a client sends. The node strips them before the guest sees the request. `/{app}/` in the path still names the app and its volumes, and the digests sent under one name are versions of one app.

What happens on the first request for a digest:
1. **Signature first.** The supervisor pulls the manifest anonymously, then the Sigstore bundles attached to it as referrers. It verifies them offline against Sigstore's public-good trusted root, embedded in the binary: an exact issuer and identity from the trust header, over the manifest digest. It also checks the version (`org.opencontainers.image.version` in the manifest) against the floor. Nothing else is downloaded until that passes.
2. **Then the blobs.** The supervisor fetches the config blob (the app's limits and env) and the `application/wasm` layer, checks them against the digests the signed manifest pins, and caches them by digest under `data_dir/registry`.
3. **Then the worker.** The app's limits are clamped to the operator's `registry_limits`, and its workers get `registry_worker`. The component goes to a fresh jailed worker, which compiles it.

**Switching versions.** A request naming a digest the app's worker isn't running switches the app. The new version is fetched and verified while the old one keeps serving. The old worker then drains: no new requests, and in-flight ones (and their response bodies) finish, bounded by its longest request time. Only then does the new worker start. Both versions share the app's volumes, so an object is never live in two workers.

**Uids.** An app's uid is the owner of its `objects/<app>` directory, or the next free one for a new app. It is stable across restarts and config changes, with no separate record.

**Publishing.** [.github/workflows/publish-guests.yml](.github/workflows/publish-guests.yml) is the reference pipeline. CI builds each test guest, pushes it with `oras` (artifact type `application/vnd.nanovm.app.v1+json`, its `nanovm.json` as the config, version `1.0.<run>`), and attaches a GitHub artifact attestation, signed with the workflow's short-lived identity. No key exists anywhere. A person can sign the same way from a laptop with `cosign` and a browser sign-in.

## Key-value and config

**`wasi:keyvalue`** (`store`, `atomics` and `batch`, 0.2.0-draft) is implemented by the host on the object's own volume. Bucket `b` is the directory `/data/.kv/b`, and each key is one file in it, named by the key in unpadded base64url so that no key can name a path.
- Values count against the object's disk budget and keys against its file limit, like any other file.
- Each value is replaced atomically: it is written beside, then renamed.
- A read is bounded by the object's memory limit, and a batch names at most 1,000 keys. Host calls aren't interrupted by the CPU budget, so these bounds keep them short.
- `wasi:filesystem` stays exposed. A guest can reach `.kv` through it; the store lists only well-formed key files, and refuses to write over anything else.

Spin's store, by comparison, is one SQLite table per app shared by all its instances, with no per-store disk quota. Ours is per object, under the object's quotas, with no dependency. It gives up SQLite's durable, atomic batches: values are not fsynced, and a batch is not atomic.

**`wasi:config/store`** (0.2.0-rc.1) serves the app's `config` table: `[app.config]` for a local app, `"config"` in a registry app's `nanovm.json`. Environment variables (`env`) still work alongside it.

## WebSockets

The supervisor holds client sockets. Objects see them as ordinary requests, in [Pushpin's WebSocket-over-HTTP](https://pushpin.org/docs/protocols/websocket-over-http/), so a guest stays a plain HTTP handler and existing GRIP libraries work.
- **Opening.** A socket opened at `/{app}/{object}/…` arrives as a `POST` of `application/websocket-events` with the event `OPEN` and a `Connection-Id`. The object accepts by answering `OPEN`; any other answer refuses the socket, with the object's own status if it is a 4xx or 5xx (so a `401` reaches the client).
- **Events.** Each later event (`TEXT`, `BINARY`, `CLOSE`, `DISCONNECT`) is a `POST` queued with the object's other requests, so events are ordered with everything else the object serves. The answer carries events for that socket.
  - A socket has one event in flight at a time, and its next message is read once that is answered. Messages for the client are written meanwhile.
  - An event waits for room in a full queue rather than being refused; the sockets bound how many wait.
  - Only the node sends events: a client request of type `application/websocket-events` is refused, so an object can trust `Connection-Id`. That type means "an event of a socket the proxy in front holds" and nothing else, so no legitimate client request carries it.
- **Fan-out within an object**, as in Durable Objects (an object is a room). An object that answers `Sec-WebSocket-Extensions: grip` sends `m:` messages and `c:` controls. `{"type":"subscribe","channel":"…"}` subscribes the socket to a channel of its own object. The object reaches every subscribed socket, from any request, by posting GRIP's publish format to `http://nanovm/publish/`; the worker stamps that request with the object's id.
- **Lifetime.** Sockets outlive instances: an evicted object is started again by the next event. They belong to the worker process, and close (1012) when it exits or the app switches versions, so clients reconnect to the new version. A node restart drops them.
- **Limits.**
  - `max_sockets` per object (default 256), per app (`[app.worker] max_sockets`, default 1,000) and per node (default 10,000; each is a file descriptor in the supervisor).
  - A message is bounded by `max_request_body_kb` (closed with 1009), and an answer by `max_response_body_kb`.
  - Messages waiting to be written count their bytes plus 64 each. A socket more than `socket_backlog_kb` (default 1,024) behind is dropped, and so is one whose write stalls for 30 s. Each socket's budget is its own, so no client or app can crowd out another; the supervisor's worst case is `max_sockets` × the largest `socket_backlog_kb`, which `registry_limits` caps.
  - A socket is pinged every 30 s and dropped if it heard nothing for a whole interval, so a half-open connection doesn't hold its slot.
  - A socket has at most 64 channels and 16 KiB of handshake headers. A publish has at most 64 items.

## Benchmarks

```bash
cargo run --release -p nanovm --example bench -- --json target/bench.json
```

Add `--quick` for a run that is 10× smaller. A full run on the same Mac gave:

| Scenario | Throughput | p50 | p99 |
|---|---|---|---|
| Cold start, 10,000 objects, 256 concurrent | 16.9K/s | 15.1 ms | 32 ms |
| Warm GET, 1,000 objects, 256 concurrent | 327K/s | 0.35 ms | 5.8 ms |
| Warm GET, one client, one object | 71K/s | 0.01 ms | 0.02 ms |
| `POST /incr` (writes `/data`), 256 concurrent | 27K/s | 8.8 ms | 21 ms |
| `POST /incr`, one client, one object | 12.0K/s | 0.08 ms | 0.12 ms |
| First request after a trap (fresh instance) | 3.8K/s | 0.19 ms | 0.27 ms |
| Churn: new object plus LRU eviction, 20,000 through 1,000 slots | 11.8K/s | 5.2 ms | 11.5 ms |
| Neighbours, 100 objects, 64 concurrent | 389K/s | 0.09 ms | 0.81 ms |
| ...the same, with 16 attackers looping | 154K/s | 0.22 ms | 2.4 ms |

Density at 10,000 live objects is 157 KB RSS per object. Instantiation p50 is at most 1 ms, from a pooling allocator and a content-addressed `.cwasm` cache.

### Reading these numbers

- **Neighbours under attack.** The 16 attackers ran 1,008 attacks in 3.3 s, and 928 instances were killed. Under that load, the benign objects kept 39% of their throughput and p99 stayed under 2.5 ms. This is the heavy-lanes scheduler at work: a request that has used 10 ms of CPU must hold one of `heavy_lanes` permits (default: half the cores) to keep running, so CPU hogs can't occupy every worker.
- **File-reading GETs are bounded by macOS.** A GET that reads an existing `/data` file runs at about 113K/s. A plain C loop of `openat`/`pread`/`close` on 14 threads manages only about 88K/s on this Mac, because macOS `openat` doesn't scale across cores. Linux path lookup does scale, so measure this on Linux.
- **RSS.** macOS RSS doesn't fully drop after eviction, so per-object RSS is better measured on Linux.

### Linux

The same benchmark on a 4-core Intel Xeon (2.1 GHz) cloud sandbox, a Firecracker microVM with no `/dev/kvm`, using `--cold 9000` because the sandbox caps fds at 20,000:

| Scenario | Linux, 4 cores | Mac, 14 cores |
|---|---|---|
| Cold start, 256 concurrent | 6.3K/s, p50 41 ms (9,000 objects) | 16.9K/s, p50 15 ms (10,000) |
| RSS per live object | **87 KB** | 157 KB |
| Warm GET, 1,000 objects, 256 concurrent | 80K/s, p50 3.1 ms | 327K/s, p50 0.35 ms |
| Warm GET, one client, one object | 11.5K/s, p50 0.07 ms | 71K/s, p50 0.01 ms |
| `POST /incr`, 256 concurrent | 8.6K/s | 27K/s |
| `POST /incr`, one client, one object | 2.0K/s, p50 0.47 ms | 12.0K/s, p50 0.08 ms |
| First request after a trap | 1.1K/s, p50 0.59 ms | 3.8K/s, p50 0.19 ms |
| Churn, 20,000 objects through 1,000 slots | 3.5K/s | 11.8K/s |
| Neighbours, 100 objects | 88K/s, p99 2.9 ms | 389K/s, p99 0.81 ms |
| ...with 16 attackers looping | 38K/s (43%), p99 4.7 ms | 154K/s (40%), p99 2.4 ms |

Every scenario completed with 0 failed requests.

- **Per core, throughput is roughly on par with the Mac.** The exception is single-client latency. In this VM a cross-core thread wakeup round trip costs 21.5 µs, against 2.3 µs on one core (measured with a futex ping-pong). Each request crosses threads twice, from the caller to the object's actor and back. That is a cost of this nested-virtualisation sandbox, so measure it again on a regular cloud VM.
- **Linux path lookup scales.** `scripts/openbench.c` (`openat` + `pread` + `close`) does 595K/s on 1 thread and 1.26M/s on 4. The Mac managed 88K/s on 14 threads, so the Mac's file-reading-GET ceiling was macOS.
- **`/incr` is slow here.** It costs about 0.5 ms per request even serially. Reads are fast, so the cost is in creating, truncating and renaming files on this VM's ext4. Not yet profiled.

### Host requirements on Linux

Each worker checks these at startup and refuses to start, naming the fix, rather than failing requests at runtime. The limits apply per worker process, so per app:

- **File descriptors:** each live object holds 2 (`/data` and `/tmp`; `/app` is shared per app). The check needs about `2 × max_live_objects + 1024`. The worker raises its soft limit to the hard limit itself, before it jails itself; the demo's 8,000 objects per app need `ulimit -n` of at least 17,088.
- **Memory mappings:** about 8 per pool slot. The default `vm.max_map_count` of 65,530 fits roughly 7,600 objects; 8,000 needs `sysctl -w vm.max_map_count=68352` or more.
- **The jail** needs Landlock (Linux 5.13+) and seccomp. A separate uid per app needs the supervisor to run as root.
- **Don't run with `RUST_BACKTRACE` set.** wasmtime then captures a stack backtrace for every guest-visible error. That includes routine ones, like reading a file that doesn't exist yet. With it set, warm throughput across 1,000 objects fell from 63K/s to 23K/s.

## Tests

```bash
scripts/build-guests.sh
cargo test --release -p nanovm
```

There are 51 integration tests, plus one that needs the internet and is ignored by default: it is the only one that makes a real HTTPS request (to example.com, through the webpki roots), and this sandbox has no DNS or direct internet. They drive real components: [guests/counter](guests/counter) and [guests/hostile](guests/hostile). The hostile guest knows every attack in the storm.

The tests cover:
- one request at a time per object, with objects running in parallel
- full queues
- every limit, and that a kill doesn't disturb neighbours
- CPU hogs not starving short requests
- volumes surviving eviction and restarts, and not being shared between objects
- `/tmp` wiped on a new instance
- file-size and file-count quotas
- read-only `/app`, sandbox escapes, and one disk budget across `/data` and `/tmp`, including renames between them
- an allowed outbound request through the full egress path
- the egress allowlist, including DNS rebinding to loopback
- LRU and idle eviction, and capacity limits
- churn past capacity
- the HTTP front end
- the jail, from a probe process inside it (`jail_blocks_escapes`): another app's files, `/etc`, sockets, fork, exec, setuid, ptrace, signals, namespaces, W+X memory and raising limits are all refused; a sleep survives a stop and continue
- the same attacks through the supervisor and jailed workers, with egress enforced by the supervisor, including an allowed request whose upstream tries to forge the supervisor's verdict header
- a killed worker restarted with its volumes intact, while its neighbour keeps serving
- registry apps against the real, CI-signed guests on ghcr.io:
  - a good signature runs
  - a wrong identity, a wrong issuer, a version below the floor and an unknown digest are each refused, with the reason
  - a local app can't be named from a registry, and the trust header is required
  - switching versions lets the old worker's in-flight request finish before the new worker answers, on the same volumes

  These skip, saying why, where ghcr.io can't be reached.
- open response bodies counting against `max_outbound_concurrency`, in-process and through the supervisor
- many requests reusing the links without errors or restarts, and dials waiting out a peer slow to accept
- workers placed in their cgroups, and the cgroup limits enforced: a worker over `memory.max` is OOM-killed and restarted while the other app keeps serving, `cpu.max` throttles, `pids.max` holds. These need cgroup v2 with those controllers and skip otherwise, saying why.
- a worker that can't reach its own directories failing to start, and saying which one
- key-value: values persisting in the volume, counting against the disk and file quotas, bad names refused, keys paged, reads bounded by the memory limit and batches by size, and the store working in a jailed worker (including a key file the guest replaced through its filesystem)
- config values served through `wasi:config`, locally and from a registry app's `nanovm.json`
- WebSockets through the supervisor: echo, binary, fan-out to the sockets of one object only (from an event and from a plain request), a guest-initiated close, the object told how a socket ended, refusals by an object that doesn't answer `OPEN`; the per-object and per-node caps, the message size, a client that never reads being dropped, and a worker's sockets closing when it dies

### Testing cgroups in a VM

Some hosts, this sandbox among them, give the memory, cpu and pids controllers to cgroup v1, so cgroup v2 limits can't be tested on them. `scripts/vm-test.sh` boots a throwaway QEMU VM (an Ubuntu kernel and a busybox initramfs holding the test binaries and guests) whose cgroup v2 has every controller, and runs the cgroup tests and `jail_blocks_escapes` there. It is for testing only: in production the supervisor runs on the host, in a cgroup subtree delegated to it (systemd `Delegate=yes`).

```bash
scripts/vm-test.sh          # cgroups and the jail
scripts/vm-test.sh all      # everything; without KVM, CPU-budget and latency tests fail under emulation
```

On Linux 7.0 (emulated, 4 vCPUs), the default set passes. Under `all`, 35 of 40 pass. The 5 that fail ask a hostile guest for real work inside a 100 ms CPU budget, which emulation can't meet.

## Bugs the tests and benchmarks found

- **`.cwasm` cache race.** Nodes compiling the same component at once wrote the same temp file, so a reader could load a half-written one. Each writer now uses a unique temp name and renames it into place.
- **CPU hogs starved neighbours.** Spinning guests are interrupted every 5 ms, but they yielded straight back onto every worker. The fix is heavy lanes, and `cpu_hogs_leave_cores_for_short_requests` fails without it.
- **Pool exhaustion under churn.** An LRU-evicted object's store lives until its actor next runs, so live stores could outnumber `max_live_objects`, and a new object could find the pool full.
  - Now every store holds one of `max_live_objects` slots, released only after the store is dropped. A new object waits until an evicted object's pool slot is actually free, and instantiation never sees a full pool.
  - `churn_past_capacity_never_fails` checks the peak number of live stores. Without the slots, it sees 25 live stores against a limit of 16.
- **Renaming a file onto itself erased its quota usage.** `rename_at` credited the size of an existing target, but renaming a file onto itself is a POSIX no-op. Repeating it zeroed the mount's byte and file counts, so a guest could write past its quota.
  - The fix compares the file identity of the source and the target, and credits only a real replacement.
  - `rename_quota_accounting` checks both cases: the self-rename credits nothing, and a rename over another file still frees its space.
- **Objects ran out of fds at a third of capacity on Linux.** Each live object held three directory fds, so a 20,000 fd limit capped the node near 6,600 objects. Every cold start after that failed with `EMFILE`. The Mac's higher fd limit hid it.
  - `/app` is now one fd per app, shared read-only, and startup checks the fd and mapping limits.
- **A CPU kill could kill the next instance at birth.** The CPU charge was reset only when a request began. Re-instantiation after a CPU kill started with the old charge already over budget, so an epoch tick during instantiation killed the new instance (a rare 503 `instantiate-failed`). A new object was also charged its time waiting for a slot.
  - Each instantiation now starts a fresh charge. `cpu_kill_does_not_poison_next_instance` fails on its first round without the fix.
- **The counter could lose its count.** `/incr` truncated `/data/count` and rewrote it in place, so a kill in between left an empty file that read back as 0. Guest writes to `/data` now go to a temporary file that is renamed into place (`util::write_file_atomic`).
- **The benchmark leaked nodes between scenarios.** Actors hold their node alive, so each scenario inherited the previous one's objects and fds. `Node::shutdown` now releases them.
- **Filesystem calls queued on one mutex.** wasmtime-wasi sends every filesystem call through tokio's `spawn_blocking`, which has a single lock. Under 256 concurrent writers it collapsed throughput 10×.
  - The fix is `allow_blocking_current_thread(true)`, which runs small local-disk calls inline and charges them to the object's CPU budget. It has to be set before the preopens, which capture it.
  - Result: concurrent `/incr` went from 3K/s to 27K/s, and warm GETs got 1.6× faster.

- **A signal could switch off CPU limits in a worker.** Stopping and continuing a jailed worker (strace did it) interrupted the epoch thread's sleep. The kernel resumes such a sleep through `restart_syscall`, which the first seccomp allowlist lacked, so the epoch thread panicked and exited. Without it, CPU budgets and deadlines go unenforced. `restart_syscall` is now allowed, and the process aborts if the epoch thread ever dies. `jail_blocks_escapes` stops and continues its probe mid-sleep.
- **HTTP/2 to the workers dropped every connection after 1,024 resets.** The first version used one HTTP/2 connection per worker. hyper's HTTP/1 front end stops reading a body once it has its Content-Length, which resets the HTTP/2 stream. h2 counts those over the connection's whole life and sends GOAWAY at 1,024. The demo saw it as a worker restarting 9 times. It was also slower: every request crossed a shared connection task in each process. Links replaced it.
- **Pooled links weren't reused.** The same early stop meant a link's response body was dropped before it saw its end, so the connection was closed instead of returned to the pool. The profile showed a socket closed per request. A link now goes back to the pool as soon as its body reports `is_end_stream`.
- **A jail rule could vanish silently.** The Landlock crate skips a path it cannot open, so a `static_dir` the app's uid couldn't reach simply lost its rule. The worker then failed to load, and the supervisor killed it before it could print why. The jail now checks every path it grants, worker stderr is tagged into the supervisor's log, and a failed start reports the worker's last words.
- **Jailed workers didn't limit open egress connections.** The per-object `max_outbound_concurrency` permit was held until the connection finished, and on the supervisor path that "connection" ended as soon as response headers arrived. A guest could hold any number of response bodies open. The permit now lives as long as the response body on both paths. An independent review found it; `open_response_bodies_count_against_outbound_concurrency` fails without the fix.
- **WebSockets, from an independent review.** Each of these was fixed with a test that fails without the fix:
  - A plain request with the events content type could forge any socket's events.
  - An empty published message cost nothing against the backlog, so a guest could grow the supervisor without limit (66 MB to 730 MB in 20 s).
  - A node-wide backlog, added for that, let one app's slow clients get other apps' sockets dropped. Each socket now has a budget of its own.
  - A burst of events from more sockets than `max_queue` was refused with `object-busy`, closing those sockets and losing their `DISCONNECT`s.
  - One app could hold every socket on the node.
  - Nothing was written to a socket while its event was in flight.
  - A drain that ran out of time left the old worker running beside the new version, so an object could be live in two workers. This dated from phase 3. The drain now kills the old worker at its deadline (`a_drain_that_runs_out_of_time_kills_the_old_worker`).
- **Debug test builds failed key-value tests on a slower machine.** Host work inside a guest call is charged to the guest's CPU budget, and unoptimized wasmtime copies a guest's `list<u8>` a byte at a time, so 8 MiB of writes took over 100 ms. Test builds are now optimized (`[profile.dev.package]` in `Cargo.toml`); release builds never had the problem.
- **Cold starts failed with `502 worker-down`.** A worker busy instantiating fell behind on its control socket, so the supervisor's non-blocking `sendmsg` failed with `EAGAIN`. Each failed request then made curl send another, which needed another new link. A dial now waits up to 5 s for the peer. The unit test `dials_wait_for_a_slow_acceptor` fails without it.

## Layout

| Path | What it is |
|---|---|
| `crates/nanovm/src/engine.rs` | Engine config, pooling allocator, epoch ticker, `.cwasm` cache |
| `crates/nanovm/src/app.rs` | Component loading and the linker |
| `crates/nanovm/src/object.rs` | The per-object actor: instance lifecycle, limits, CPU accounting, heavy lanes, poisoning |
| `crates/nanovm/src/table.rs` | The node: `(app, id)` → actor, LRU and idle eviction |
| `crates/nanovm/src/quota.rs` | Per-mount byte and file quotas over WASIp3 filesystem calls |
| `crates/nanovm/src/egress.rs` | Allowlist plus resolved-IP filtering for outgoing HTTP |
| `crates/nanovm/src/server.rs` | A node's HTTP surface: routing, `/_nanovm/stats` and `/_nanovm/events` |
| `crates/nanovm/src/supervisor.rs` | The supervisor (front end, workers, egress on their behalf) and the worker's entry point |
| `crates/nanovm/src/jail.rs` | uid, Landlock and seccomp, entered by a worker before any guest code |
| `crates/nanovm/src/registry.rs` | Registry apps: pull by digest, Sigstore verification, version floor, digest cache |
| `crates/nanovm/src/link.rs` | HTTP/1.1 over socketpairs passed across a control socket |
| `crates/nanovm/src/kv.rs` | `wasi:keyvalue` on the object's volume |
| `crates/nanovm/src/ws.rs` | WebSockets: the supervisor's sockets, WebSocket-over-HTTP events and per-object channels |
| `crates/nanovm/support/load.rs` | In-process load generator shared by the benchmark and the tests (not part of the node) |
| `guests/` | `counter`, `hostile` and shared `util` (wasm32-wasip2 with wasip3 bindings) |

## Known limits of this phase

- **Inline filesystem calls can't be interrupted by the epoch.** Each one is bounded by the quotas and charged to the guest's CPU budget.
- **The CPU budget is wall time inside each poll of the guest.** It inflates when the host is oversubscribed, which errs on the side of killing hogs early.
- **Without root, apps share the supervisor's uid.** Landlock and seccomp still separate them.
