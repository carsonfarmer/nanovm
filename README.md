# nanovm

A wasmtime-only host for untrusted WASIp3 HTTP components, run as Durable-Objects-style objects:

- Each `(app, object-id)` has exactly one live instance, which serves one request at a time.
- Each object has its own memory cap, CPU budget, deadlines and handle cap.
- Each object has its own quota'd filesystem: scratch `/tmp` and a persistent `/data` volume.
- Each app has an egress allowlist.
- Each object has a `wasi:keyvalue` store in its volume, and each app its `wasi:config` values.
- Objects accept WebSockets and publish to their own sockets.
- Memory is lost on eviction; `/data` survives eviction and restarts.

Apps are signed artifacts in OCI registries. A proxy finds each host's app in DNS; each app runs in its own jailed worker process, under a supervisor that runs no guest code and owns the network (see [The jail](#the-jail)). [PLAN.md](PLAN.md) has the design and what comes next.

## Try it

```bash
demo/demo.sh
```

The script:
1. Builds the guests and the node.
2. Publishes a benign `counter` app and a `hostile` app to a registry on loopback (`cargo run --example dev-registry`).
3. Starts a node in dev mode from [demo/nanovm.toml](demo/nanovm.toml), which runs them unsigned.
4. Pings a canary object throughout.
5. Drives it over HTTP with `curl --parallel`: per-object volumes, path escapes and egress, 5,000 cold starts, 50,000 warm requests, `/data` writes, and a storm of 100 simultaneous attacks.

It leaves the node running. Its data APIs (deprecated: OpenTelemetry replaces them, see [ISSUES.md](ISSUES.md) W-2) are `GET /_nanovm/stats` (the supervisor's view plus each app worker's counters and latency histograms, as JSON) and `GET /_nanovm/events?app=A&since=N` (app `A`'s kills, denials and evictions).

A request names its app in the headers the proxy sets (see [Apps](#apps)), and its object in the path, `/{object}/{path}`:

```bash
curl -XPOST http://127.0.0.1:8080/alice/incr \
  -H 'nanovm-app: 127.0.0.1:5000/nanovm/counter@sha256:…' -H 'nanovm-trust: dev dev'
```

## What a run of `demo/demo.sh` shows

On the 4-core Linux sandbox, with each app's worker capped at 8,000 objects (the sandbox's fd limit; see Host requirements). These numbers go through curl, which shares the 4 cores, and the HTTP front end; the in-process numbers are under Benchmarks. The second column is the same demo on the unjailed single-process node from before the jail, on the same machine.

| Scenario | Jailed | Unjailed |
|---|---|---|
| 5,000 cold starts (64 in flight) | 3.7K/s, p50 14 ms, 89 KB RSS per new object | 4.0K/s, p50 14 ms, 87 KB |
| 50,000 GETs over 1,000 warm objects | 19K/s, p50 2.2 ms | 27K/s, p50 1.2 ms |
| 10,000 `POST /incr` (each one rewrites `/data/count`) | 6.7K/s | 7.3K/s |
| One object, one client | 3.0K/s, p50 0.3 ms | 5.6K/s, p50 0.1 ms |
| 110 attacks at once (now 100: `/app` is gone) | All contained in 2.2 s; the 2 s wall-clock deadline sets the slowest | the same |
| Canary over HTTP, throughout | 0 failures | 0 failures |

Every request succeeded and each worker started once. The cost of the jail is the hop between the supervisor and the worker; see [What the jail costs](#what-the-jail-costs).

A run in a 4-core Amp orb after phase 7 (the registry setup, with per-request commits): 5,000 cold starts at 2.9K/s (p50 18 ms, 95 KB per new object), 50,000 warm GETs at 15.9K/s (p50 2.7 ms), 10,000 `POST /incr` at 600/s (each now fsyncs its commit; 1 of them was a CPU kill under load), one client at 2.9K/s (p50 0.3 ms), all 100 attacks contained in 2.2 s, and 0 canary failures.

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
| `../` and symlink escapes | refused |
| fetch cloud metadata (169.254.169.254) | denied (not on the allowlist) |
| allowlisted name that resolves to 127.0.0.1 | denied by the resolved-IP check (in this sandbox, which has no DNS, it fails to resolve instead; the `localhost` integration test covers the IP check) |

## The jail

```
client ─► proxy ─► supervisor (nanovm serve): runs no guest code
                    │  HTTP/1.1 front end; routes each request to its app's worker
                    │  makes every outbound request: allowlist, DNS, public-IP filter
                    │  starts and restarts workers; places each in its cgroup
                    ├── fd 3: links for requests ──► worker "counter"  (nanovm worker)
                    └── fd 4: links for egress  ◄──  one Node for one app, jailed
```

A worker reads its config and component from stdin, then jails itself while it still has one thread, before it compiles or runs anything:

- **Its own uid.** When the supervisor runs as root, app `i` runs as `jail_uid_base + i`, and its directories are chowned to it.
- **Landlock.** It gets read-write access to the app's own `objects/`, `tmp/` and `cache/`, and nothing else.
- **seccomp.** The allowlist holds only the syscalls Wasmtime, tokio and the filesystem host make, traced over the test suite:
  - no `socket`, `connect`, `bind`, `execve`, `ptrace`, `mount` or namespace calls
  - `clone` for threads only
  - no memory that is writable and executable at once
  - limits can be read, never raised
  - `fcntl` for descriptor flags only: no `F_SETOWN` or `F_SETSIG`, which would have the kernel signal another process of the same uid

  Anything else fails with `ENOSYS`. With no way to create a socket, a worker's only channels are its two control sockets to the supervisor.
- **`no_new_privs`**, and the worker dies with its supervisor.
- **cgroup v2** (optional, `cgroup = "<delegated dir>"`). Each worker goes into `<cgroup>/<app>` with the node's `[worker]` memory, CPU and pids limits, before it is given anything.

Mount and network namespaces were left out on purpose: Landlock already confines the filesystem, and with no `socket()` there is no network to remove.

**Links.** The supervisor and a worker talk HTTP/1.1, one request at a time per connection, over socketpairs. The dialing side passes one end of each pair to the other over its control socket (`SCM_RIGHTS`) and keeps the connections it has made in a pool. A request runs inline in the task that sent it, at both ends: on this path every handoff between tasks is a thread wakeup, and those cost more than the HTTP framing. Each front-end thread (`front_threads`, a quarter of the cores by default) has its own runtime, listener and pool. The supervisor serves a worker at most as many egress links as its objects can use at once (`max_live_objects × (max_outbound_concurrency + 1)`), and all workers together at most a quarter of its file descriptor limit, which it raises to the hard limit at start; a link past either cap is closed unserved.

A worker that dies takes only its app with it. In-flight requests get `502 worker-down`, and the next request starts a fresh worker. Volumes survive.

### What the jail costs

The hop between the processes costs two extra cross-process round trips, about 30–40 µs each in this VM, plus one more HTTP parse. On the 4-core sandbox:

- One client sees about 0.1 ms more latency, the warm GET p50 above.
- At 64 in flight, throughput is about 25% lower. The supervisor needs about a fifth of a request's CPU, and here it shares 4 cores with the worker and with curl.

The in-process benchmark below measures the node inside a worker, and is unchanged.

## The proxy

`nanovm proxy --config proxy.toml` is the public front door. It finds the app a request's host serves in a TXT record at `_nanovm.<host>`:

```
app=ghcr.io/org/app:prod; trust=<issuer> <subject>; trust=…; min=1.2.0
```

That is the repository and the tag deployed, each identity whose signature is accepted, and an optional version floor. Whoever controls the domain controls its routing; deploying moves the tag, and rolling back moves it back. The proxy:
- asks a DNS-over-HTTPS resolver (`resolver`, Cloudflare's by default) and keeps the answer for its TTL;
- asks the registry which digest the tag names, and keeps that for `tag_ttl_s` (30 s);
- looks each host or tag up once however many requests ask at once, keeps a failed lookup's answer for 5 s rather than asking again, and remembers at most 10,000 of each;
- drops every `nanovm-*` header a client sent, and hop-by-hop headers (`Connection` and what it lists, `Keep-Alive`, `TE`, `Trailer`, `Proxy-Authorization`), names the app to the node (below), and forwards the request, WebSockets included;
- speaks plain HTTP: TLS is left to a terminator in front (Caddy, a cloud load balancer, Cloudflare), which also renews and reloads certificates;
- gives a client 10 s for each request's headers (or between requests), and waits out running out of file descriptors rather than exiting (the node's front end does too).

A host with no record is a 404; a malformed record, or more than one, is a 502.

DNS as the routing table has limits: a record is as trustworthy as its resolver (hence DNS-over-HTTPS, and apps keyed by the name that owns their record, so a record reaches no other name's app); a change takes up to its TTL to arrive; a host serves one app (several hosts can share one through CNAMEs); the table is public; and a cold lookup adds a round trip.

## Apps

Every app comes from a registry. The proxy names it with each request:

```
nanovm-app: ghcr.io/org/app@sha256:…        # by digest only
nanovm-trust: <issuer> <subject>[, …]        # whose signature is accepted
nanovm-min-version: 1.4.0                    # optional floor
```

The node trusts these headers because only the proxy can reach it, and strips them before the guest sees the request. A request without them reaches only the node's own `/_nanovm/` routes.

**An app is a DNS name's deployment of a repository.** Its name on the node (`example.com_ghcr.io_org_app-<hash>`), its volumes and its uid come from the name that owns its `_nanovm` record (`nanovm-host`) and `registry/repository`, so every version that name runs, whoever signed it, sees the same data. The owner is the name the record was found at after CNAMEs, so `_nanovm.www.example.com CNAME _nanovm.example.com` makes both hosts one app. Whoever controls that name chooses the version it runs, so no domain can name another's repository and switch its app to an old version on its data (a review finding); a CNAME to someone else's record only serves their app as their record configures it. Only those who can push to a repository can put a signed artifact in it, so naming another identity never reaches a different app's data. The registry is written one way only: lower case, without `:443`. A team signs from CI, or lists each member's identity.

What happens on the first request for a digest:
1. **Signature first.** The supervisor pulls the manifest anonymously, then the Sigstore bundles attached to it as referrers. It verifies them offline against Sigstore's public-good trusted root, embedded in the binary: an exact issuer and identity from the trust header, over the manifest digest. It also checks the version (`org.opencontainers.image.version` in the manifest) against the floor. Nothing else is downloaded until that passes.
2. **Then the blobs.** The supervisor fetches the config blob (the app's limits and env) and the `application/wasm` layer, checks them against the digests the signed manifest pins, and caches them by digest under `data_dir/registry`.
3. **Then the worker.** The app's limits are clamped to the operator's `limits`, and its worker gets `worker`. The component goes to a fresh jailed worker, which compiles it.

**A running version under a new policy** (other identities, or a higher floor) is checked against it before the worker serves it; a refusal leaves the version running. The supervisor remembers up to 16 accepting policies per running version, then forgets them and checks each again as it comes.

**How many apps.** A node runs at most `max_apps` workers (100). A new app takes the slot of the least recently used idle app, one with no requests in flight and no open sockets, whose worker is retired (its next request starts it again). With none idle, the new app is refused (502) until one is. The node forgets apps with no worker once it knows of `2 × max_apps`. Since anyone's DNS can name an app, this is what keeps a node's processes, uids and memory bounded; it does not stop one party from keeping every slot busy, which needs per-client limits in the proxy.

**Bounds.** Every registry read is bounded: a manifest, config, token or signature at 4 MiB, a component at `max_component_mb` (64), and 16 signatures per load. At most four registry fetches run at once across the node, each within `load_timeout_ms` (60 s) in all, so a registry that answers a byte at a time holds nothing for long. Digests are checked before they name anything. Registries, and their redirects and token servers, must be on public addresses: a deploy names the registry, so it must not reach the node's own network.

**Dev mode** (`dev = true`, refused unless the binary was built with `--features dev` and `listen` is loopback, so a default release build can't be put in dev mode) is for local development: apps need no signature, registries and egress entries may be on private addresses, and loopback registries are plain HTTP. The tests and the demo use it with a registry on loopback; signatures are tested against the CI-signed guests on ghcr.io.

**Switching versions.** A request naming a digest the app's worker isn't running switches the app. The new version is fetched and verified while the old one keeps serving. The old worker then drains: no new requests, and in-flight ones (and their response bodies) finish, bounded by its longest request time. It is then killed, and only once its process has exited does the new worker start. Both versions share the app's volumes, so an object is never live in two workers. A switch, once begun, finishes even if the request that began it goes away.

**Uids.** An app's uid is the owner of its `objects/<app>` directory, or the next free one for a new app. It is stable across restarts, with no separate record.

**Publishing.** [.github/workflows/publish-guests.yml](.github/workflows/publish-guests.yml) is the reference pipeline. CI builds each test guest, pushes it with `oras` (artifact type `application/vnd.nanovm.app.v1+json`, its `nanovm.json` as the config, version `1.0.<run>`), and attaches a GitHub artifact attestation, signed with the workflow's short-lived identity. No key exists anywhere. A person can sign the same way from a laptop with `cosign` and a browser sign-in.

## Key-value and config

**`wasi:keyvalue`** (`store`, `atomics` and `batch`, 0.2.0-draft) is implemented by the host on the object's own volume. Bucket `b` is the directory `/data/.kv/b`, and each key is one file in it, named by the key in unpadded base64url so that no key can name a path.
- Values count against the object's disk budget and keys against its file limit, like any other file.
- A value is written in place; its request's commit makes it durable, together with everything else the request changed (see Commits).
- A read is bounded by the object's memory limit, and a batch names at most 1,000 keys. Host calls aren't interrupted by the CPU budget, so these bounds keep them short.
- `wasi:filesystem` stays exposed. A guest can reach `.kv` through it; the store lists only well-formed key files, and refuses to write over anything else.

Spin's store, by comparison, is one SQLite table per app shared by all its instances, with no per-store disk quota. Ours is per object, under the object's quotas, with no dependency. Commits give it what SQLite would: a request's values, files included, land together and durably, or not at all.

**`wasi:config/store`** (0.2.0-rc.1) serves the `"config"` table of the app's `nanovm.json`. Environment variables (`env`) still work alongside it.

## Calls between objects

An object calls another object of its own app with an ordinary outgoing request to `http://nanovm/objects/{id}/{path}`, as a Durable Object calls a stub:
- The callee gets it as a request for `/{path}`, through its queue like any other, so it is still served one request at a time.
- The callee commits before it answers, so the caller only ever sees durable changes. Each object commits on its own: a caller that fails afterwards doesn't undo the callee's changes.
- Calls need no egress rule. They count against the caller's `max_outbound_concurrency`, and their bodies against the app's `max_request_body_kb`.
- **Cycles are refused.** With one request at a time per object, a call that waits on its own caller, directly (`a` calls `a`) or through other calls (`a` calls `b`, which calls `a`), could never be served. Each object records the objects its calls wait on. A call that would close a cycle gets `508 call-cycle` at once, instead of hanging until a deadline kills both requests and rolls them back. The check and the record happen under the table lock, so two objects calling each other at the same moment can't both get through.

## WebSockets

The supervisor holds client sockets for now. They are planned to move into the front proxy, so the node only ever answers requests ([ISSUES.md](ISSUES.md) W-3).

The supervisor holds client sockets. Objects see them as ordinary requests, in [Pushpin's WebSocket-over-HTTP](https://pushpin.org/docs/protocols/websocket-over-http/), so a guest stays a plain HTTP handler and existing GRIP libraries work.
- **Opening.** A socket opened at `/{object}/…` arrives as a `POST` of `application/websocket-events` with the event `OPEN` and a `Connection-Id`. The object accepts by answering `OPEN`; any other answer refuses the socket, with the object's own status if it is a 4xx or 5xx (so a `401` reaches the client).
- **Events.** Each later event (`TEXT`, `BINARY`, `CLOSE`, `DISCONNECT`) is a `POST` queued with the object's other requests, so events are ordered with everything else the object serves. The answer carries events for that socket.
  - A socket has one event in flight at a time, and its next message is read once that is answered. Messages for the client are written meanwhile.
  - An event waits for room in a full queue rather than being refused; the sockets bound how many wait.
  - Only the node sends events: a client request of type `application/websocket-events` is refused, so an object can trust `Connection-Id`. That type means "an event of a socket the proxy in front holds" and nothing else, so no legitimate client request carries it.
- **Fan-out within an object**, as in Durable Objects (an object is a room). An object that answers `Sec-WebSocket-Extensions: grip` sends `m:` messages and `c:` controls. `{"type":"subscribe","channel":"…"}` subscribes the socket to a channel of its own object. The object reaches every subscribed socket, from any request, by posting GRIP's publish format to `http://nanovm/publish/`; the worker stamps that request with the object's id.
- **Lifetime.** Sockets outlive instances: an evicted object is started again by the next event. They belong to the worker process, and close (1012) when it exits or the app switches versions, so clients reconnect to the new version. A node restart drops them.
- **Limits.**
  - `max_sockets` per object (default 256), per app (`[worker] max_sockets`, default 1,000) and per node (default 10,000; each is a file descriptor in the supervisor).
  - A message is bounded by `max_request_body_kb` (closed with 1009), and an answer by `max_response_body_kb`.
  - Messages waiting to be written count their bytes plus 64 each. A socket more than `socket_backlog_kb` (default 1,024) behind is dropped, and so is one whose write stalls for 30 s. Each socket's budget is its own, so no client or app can crowd out another; the supervisor's worst case is `max_sockets` × the largest `socket_backlog_kb`, which `limits` caps.
  - A socket is pinged every 30 s and dropped if it heard nothing for a whole interval, so a half-open connection doesn't hold its slot.
  - A socket has at most 64 channels and 16 KiB of handshake headers. A publish has at most 64 items.

## Commits

A request's changes to `/data` (files and key-value alike) and what it publishes to WebSockets take effect together when it answers, and are durable before anyone hears of them: Durable Objects' output gate. If the request fails, they never happened. "Fails" means a guest error, a trap, the CPU or time limit, or the worker crashing. An answer of any status commits.

**How:** an undo journal (`journal.rs`), as SQLite's rollback journal, under our `wasi:filesystem` host (`quota.rs`).
- **Before each change, the host records what the change would destroy, and fsyncs it:**
  - a file's size, the first time it is written;
  - the bytes a write or truncation overwrites;
  - "remove this", for a new entry;
  - "rename it back", for a rename;
  - "recreate this", for a removed directory.

  A removed or replaced file is renamed into the journal, not deleted.
- **Then the change happens in place,** so open files and `mmap` keep working, and SQLite can sit on `/data`.
- **Commit** (before the response is released, and again after post-return work) fsyncs what changed and deletes the log. Held publishes are sent after it.
- **Rollback** has one path. A failed request ends its instance, and every instance start replays any log left behind backwards, so a crash is handled the same way. Recovery trims each step off the log as it completes, so a crash during recovery resumes where it stopped.
- **Costs:**
  - One fsync per record that destroys data (first write to a file, overwrite, truncate, remove, rename), and one round of fsyncs per committing request. Requests that change nothing pay nothing.
  - Fsync time isn't charged to the guest's CPU budget.
  - The quota counts committed state. While a request runs, the log and the files it removed may each hold as much again, so an object's disk use peaks at three times its quota. Past that, a change is refused as a full disk.
- **Safety:**
  - Guest paths reach only the volume. The journal sits beside it, out of reach.
  - An undo that can't be applied is skipped rather than leave the object unable to start.
- **`/tmp` is not journaled.** It is per-instance scratch, deleted with the instance, so it's the fast place for temporary files. A rename between `/tmp` and `/data` fails as cross-device, as between real filesystems.

**Alternatives considered:**
- Key-value-only storage: rejected, real files are wanted.
- FUSE or kernel overlay filesystems: rejected. They need mounts per object, and their commit copies.
- btrfs or ZFS snapshots: rejected. They tie nanovm to one filesystem, and a snapshot costs a transaction commit per request.
- Swapping in a shadow copy of the volume: rejected. It copies the tree and breaks files held open.
- Per-file copies with reflink: rejected. They copy whole files on ext4.
- Crates (`fs-transaction`, `txfs`, `kaish-vfs`, `eryx-vfs`, `overlayfs_fuse`): none sits behind POSIX writes through open descriptors.

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

- **File descriptors:** each live object holds 2 (`/data` and `/tmp`). The check needs about `2 × max_live_objects + 1024`. The worker raises its soft limit to the hard limit itself, before it jails itself; the demo's 8,000 objects per app need `ulimit -n` of at least 17,088.
- **Memory mappings:** about 8 per pool slot. The default `vm.max_map_count` of 65,530 fits roughly 7,600 objects; 8,000 needs `sysctl -w vm.max_map_count=68352` or more.
- **The jail** needs Landlock (Linux 5.13+) and seccomp. A separate uid per app needs the supervisor to run as root.
- **Don't run with `RUST_BACKTRACE` set.** wasmtime then captures a stack backtrace for every guest-visible error. That includes routine ones, like reading a file that doesn't exist yet. With it set, warm throughput across 1,000 objects fell from 63K/s to 23K/s.

## Tests

```bash
scripts/build-guests.sh
cargo test --release -p nanovm
```

There are 62 integration tests and 92 unit tests, plus one that needs the internet and is ignored by default: it is the only one that makes a real HTTPS request (to example.com, through the webpki roots), and this sandbox has no DNS or direct internet. They drive real components: [guests/counter](guests/counter) and [guests/hostile](guests/hostile). The hostile guest knows every attack in the storm.

The tests cover:
- one request at a time per object, with objects running in parallel
- full queues
- every limit, and that a kill doesn't disturb neighbours
- CPU hogs not starving short requests
- volumes surviving eviction and restarts, and not being shared between objects
- `/tmp` wiped on a new instance
- file-size and file-count quotas
- sandbox escapes, and one disk budget across `/data` and `/tmp`, and renames between them refused as cross-device
- commits:
  - a request that errors, traps or runs out of CPU leaves its files and key-value writes untouched, while one that answers, even with 507, commits;
  - a worker killed mid-request is rolled back when the object next starts;
  - a publish followed by a panic reaches no socket;
  - unit tests roll back every kind of change, commit them, survive a crash at each step of recovery, ignore a torn record, journal writes to a file removed while open and to one opened write-only, and record nothing for a failed `rmdir` or a refused change
- calls between objects: the callee's volume and commit, and cycles (an object calling itself, or back through another) refused at once
- an allowed outbound request through the full egress path
- the egress allowlist, including DNS rebinding to loopback
- LRU and idle eviction, and capacity limits
- churn past capacity
- the HTTP front end
- the jail, from a probe process inside it (`jail_blocks_escapes`): another app's files, `/etc`, sockets, fork, exec, setuid, ptrace, signals (sent, or arranged through `F_SETOWN`), namespaces, W+X memory and raising limits are all refused, while its own directory and the cwd are reachable; a sleep survives a stop and continue
- the same attacks through the supervisor and jailed workers, with egress enforced by the supervisor, including an allowed request whose upstream tries to forge the supervisor's verdict header
- a killed worker restarted with its volumes intact, while its neighbour keeps serving
- registry apps against the real, CI-signed guests on ghcr.io:
  - a good signature runs
  - a wrong identity, a wrong issuer, a version below the floor and an unknown digest are each refused, with the reason
  - a running version is checked again under each new policy, a refusal is not remembered and leaves it running, and the trust header is required
  - switching versions lets the old worker's in-flight request finish before the new worker answers, on the same volumes

  These skip, saying why, where ghcr.io can't be reached.
- open response bodies counting against `max_outbound_concurrency`, in-process and through the supervisor
- many requests reusing the links without errors or restarts, and dials waiting out a peer slow to accept
- workers placed in their cgroups, and the cgroup limits enforced: a worker over `memory.max` is OOM-killed and restarted while the other app keeps serving, `cpu.max` throttles, `pids.max` holds. These need cgroup v2 with those controllers and skip otherwise, saying why.
- apps from a registry on loopback, in dev mode: downloads bounded by `max_component_mb` and 4 MiB of metadata, a floor checked against the running version, a switch that finishes when its request goes away, at most `max_apps` workers (taking only idle apps' slots), and slow registries held to four fetches at once and a deadline
- the proxy hanging up on clients that stall their headers, and surviving running out of file descriptors
- the proxy: a host's DNS record names its app at the version its tag names now, a client can name no app itself nor reach `/_nanovm/`, a missing or bad record is refused, moving the tag deploys, and WebSockets pass through
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
- **Commits, from an independent review.** Each fix has a test that fails without it.
  - A guest could reach its own journal through `..`, plant records, and make recovery delete files outside its volume.
  - An ordinary `mkdir` on an existing directory recorded "remove this", so a failed request deleted the directory, or bricked the object if it wasn't empty.
  - A relative `data_dir` (the demo's) silently turned the journal off.
  - A replaced file's size was recorded per path, so renaming another file onto that path escaped the undo.
  - A kv write refused for space deleted the old value.
  - The journal had no bound.
  - My own crash test also found that replaying a recovery from the start could undo a rename twice and lose a file; recovery now trims each step off the log.
- **From a second review** (listed in [ISSUES.md](ISSUES.md)). Each fix has a test that fails without it.
  - A failed `rmdir` was journaled anyway, so rolling back created a directory the file count never saw; repeated, it went past `max_files` (N-5).
  - When the full journal refused a file's size record, the file was still marked as recorded, so a retried append went through with nothing to undo it (N-6).
  - Overwriting a file opened write-only failed: the journal read the old bytes through the guest's descriptor (N-7).
  - Truncating a file after removing it, while still open, freed its bytes a second time, so committed data could exceed the quota (N-8).
  - A key-value replacement that the full journal refused kept its quota adjustment, so usage could fall below what the volume held. Past the reservation, a failure now traps and the request rolls back (N-9).
  - The jail allowed every `fcntl` command. With `F_SETOWN` and `O_ASYNC`, a worker that got out of Wasm could have the kernel signal (and kill) the supervisor or other apps' workers whenever they share its uid (a supervisor not running as root). Only the five commands the test suite uses are allowed now (N-2).
  - The demo failed every request with `503 instantiate-failed`. A worker canonicalizes its object's directory, which for a relative `data_dir` reads the cwd, and the jail refused `getcwd`. The tests use absolute paths, so only the demo hit it (N-21).
  - LRU eviction could pick an object that was idle but had a job queued, or a socket event waiting for room. Its actor drains its queue before exiting, so the next request started a second actor for the same object on the same volume. Only an object with no work queued or waiting is evicted now (N-10).
- **Cold starts failed with `502 worker-down`.** A worker busy instantiating fell behind on its control socket, so the supervisor's non-blocking `sendmsg` failed with `EAGAIN`. Each failed request then made curl send another, which needed another new link. A dial now waits up to 5 s for the peer. The unit test `dials_wait_for_a_slow_acceptor` fails without it.

## Layout

| Path | What it is |
|---|---|
| `crates/nanovm/src/engine.rs` | Engine config, pooling allocator, epoch ticker, `.cwasm` cache |
| `crates/nanovm/src/app.rs` | Component loading and the linker |
| `crates/nanovm/src/object.rs` | The per-object actor: instance lifecycle, limits, CPU accounting, heavy lanes, poisoning |
| `crates/nanovm/src/table.rs` | The node: one app, object id → actor, LRU and idle eviction |
| `crates/nanovm/src/quota.rs` | Per-mount byte and file quotas over WASIp3 filesystem calls |
| `crates/nanovm/src/egress.rs` | Allowlist plus resolved-IP filtering for outgoing HTTP |
| `crates/nanovm/src/server.rs` | A node's HTTP surface: routing, `/_nanovm/stats` and `/_nanovm/events` |
| `crates/nanovm/src/supervisor.rs` | The supervisor (front end, workers, egress on their behalf) and the worker's entry point |
| `crates/nanovm/src/jail.rs` | uid, Landlock and seccomp, entered by a worker before any guest code |
| `crates/nanovm/src/registry.rs` | Apps: bounded registry reads, Sigstore verification, version floor, digest cache |
| `crates/nanovm/src/proxy.rs` | `nanovm proxy`: DNS routing, tag resolution, forwarding |
| `crates/nanovm/examples/dev-registry.rs` | A loopback registry with the guests published, for dev mode |
| `crates/nanovm/src/link.rs` | HTTP/1.1 over socketpairs passed across a control socket |
| `crates/nanovm/src/kv.rs` | `wasi:keyvalue` on the object's volume |
| `crates/nanovm/src/journal.rs` | Per-request commits: the undo journal, commit and crash recovery |
| `crates/nanovm/src/ws.rs` | WebSockets: the supervisor's sockets, WebSocket-over-HTTP events and per-object channels |
| `crates/nanovm/support/load.rs` | In-process load generator shared by the benchmark and the tests (not part of the node) |
| `guests/` | `counter`, `hostile` and shared `util` (wasm32-wasip2 with wasip3 bindings) |

## Known limits of this phase

- **Inline filesystem calls can't be interrupted by the epoch.** Each one is bounded by the quotas and charged to the guest's CPU budget.
- **The CPU budget is wall time inside each poll of the guest.** It inflates when the host is oversubscribed, which errs on the side of killing hogs early.
- **Without root, apps share the supervisor's uid.** Landlock and seccomp still separate them.
- **A worker's stderr lines are passed on whole up to 4 KiB**; longer ones are split.
