# nanovm

A wasmtime-only host for untrusted WASIp3 HTTP components, run as Durable-Objects-style objects:

- Each `(app, object-id)` has exactly one live instance, which serves one request at a time.
- Each object has its own memory cap, CPU budget, deadlines and handle cap.
- Each object has its own quota'd filesystem: read-only `/app`, scratch `/tmp`, and a persistent `/data` volume.
- Each app has an egress allowlist.
- Memory is lost on eviction; `/data` survives eviction and restarts.

This is the portable core. It runs natively on macOS and Linux. The Linux jail (seccomp, Landlock, cgroups, a process per tenant) is the next phase. [PLAN.md](PLAN.md) has the design.

## Try it

```bash
demo/demo.sh
```

The script:
1. Builds the guests and the node.
2. Starts a node from [demo/nanovm.toml](demo/nanovm.toml), with a benign `counter` app and a `hostile` app on the same host.
3. Pings a canary object throughout.
4. Drives it over HTTP with `curl --parallel`: per-object volumes, path escapes and egress, 5,000 cold starts, 50,000 warm requests, `/data` writes, and a storm of 110 simultaneous attacks.

It leaves the node running. http://127.0.0.1:8080/_nanovm/ is a plain page showing the node's stats and event log.

Requests are routed as `/{app}/{object}/{path}`:

```bash
curl -XPOST http://127.0.0.1:8080/counter/alice/incr
```

## What a run of `demo/demo.sh` shows

On the 4-core Linux sandbox, with a node capped at 9,400 objects (the sandbox's fd limit; see Host requirements). These numbers go through curl and the HTTP front end; the in-process numbers are under Benchmarks.

| Scenario | Result |
|---|---|
| 5,000 cold starts (64 in flight) | 3.8K/s, p50 14 ms, 85 KB RSS per new object |
| 50,000 GETs over 1,000 warm objects | 24K/s, p50 1.3 ms |
| 10,000 `POST /incr` (each one rewrites `/data/count`) | 7.2K/s |
| One object, one client | 4.9K/s, p50 0.1 ms |
| 110 attacks at once | All contained in 2.1 s; the 2 s wall-clock deadline sets the slowest |
| Canary over HTTP, throughout | 0 failures |

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

`Node::new` checks these at startup and refuses to start, naming the fix, rather than failing requests at runtime:

- **File descriptors:** each live object holds 2 (`/data` and `/tmp`; `/app` is shared per app). The check needs about `2 × max_live_objects + 1024`. The node raises its soft limit to the hard limit itself; the demo's 20,000 objects need `ulimit -n` of at least 41,088.
- **Memory mappings:** about 8 per pool slot. The default `vm.max_map_count` of 65,530 fits roughly 7,600 objects; 20,000 needs `sysctl -w vm.max_map_count=164352` or more.
- **Don't run with `RUST_BACKTRACE` set.** wasmtime then captures a stack backtrace for every guest-visible error. That includes routine ones, like reading a file that doesn't exist yet. With it set, warm throughput across 1,000 objects fell from 63K/s to 23K/s.

## Tests

```bash
scripts/build-guests.sh
cargo test --release -p nanovm
```

There are 30 integration tests, plus one that needs the internet and is ignored by default. They drive real components: [guests/counter](guests/counter) and [guests/hostile](guests/hostile). The hostile guest knows every attack in the storm.

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

## Layout

| Path | What it is |
|---|---|
| `crates/nanovm/src/engine.rs` | Engine config, pooling allocator, epoch ticker, `.cwasm` cache |
| `crates/nanovm/src/app.rs` | Component loading and the linker |
| `crates/nanovm/src/object.rs` | The per-object actor: instance lifecycle, limits, CPU accounting, heavy lanes, poisoning |
| `crates/nanovm/src/table.rs` | The node: `(app, id)` → actor, LRU and idle eviction |
| `crates/nanovm/src/quota.rs` | Per-mount byte and file quotas over WASIp3 filesystem calls |
| `crates/nanovm/src/egress.rs` | Allowlist plus resolved-IP filtering for outgoing HTTP |
| `crates/nanovm/src/server.rs` | HTTP front end, `/_nanovm/stats` and `/_nanovm/events` |
| `dashboard/index.html` | A plain page over the stats and event log |
| `crates/nanovm/support/load.rs` | In-process load generator shared by the benchmark and the tests (not part of the node) |
| `guests/` | `counter`, `hostile` and shared `util` (wasm32-wasip2 with wasip3 bindings) |

## Known limits of this phase

- **Inline filesystem calls can't be interrupted by the epoch.** Each one is bounded by the quotas and charged to the guest's CPU budget.
- **The CPU budget is wall time inside each poll of the guest.** It inflates when the host is oversubscribed, which errs on the side of killing hogs early.
- **There is no OS-level jail yet.** Isolation is wasmtime's, plus the host's capability checks.
