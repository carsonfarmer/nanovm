# Round 3: what changed, why, and how to pick it

Round 3 worked from five priorities: land the security fixes, cut the cost of durable writes,
get committed state off the node, run on many nodes, and give developers an on-ramp. Each piece
was built on its own branch, measured, and integrated into two branches:

- **`cf/main-next`**: fixes only. It is `origin/main` (59b3783) plus security and correctness
  fixes. Land it first: every commit is small, has a test that fails without it, and CI passes.
- **`cf/round3`**: `cf/main-next` plus the verified improvements below. CI passes on it, including
  the SQLite durability suite, the restic backup drill, JavaScript, `nanovm run`, the 22 example
  apps, and the multi-node tests.

Numbers come from one shared 4-core VM that was often under heavy load (load average 10–30).
Where a number is CPU per request, it is process CPU from `/proc`, alternating builds, medians
over at least three rounds. Read them as before/after on the same machine.

## 1. `cf/main-next`: fixes to land on main

| Commit | Fix | Why it matters | Runtime lines |
|---|---|---|---|
| f84fa93 | Build only the dependency features the node uses | fewer crates and a smaller binary | 0 |
| 4a2778d | N-70: only stream socketpairs in the jail | a datagram socketpair could `sendto` any socket on the host | +1 |
| 0f79ca1, 1f89e09 | N-71: refuse forged socket events however the type is spelled | a client could inject WebSocket events into objects | small |
| 573b7cc | N-72: parse GRIP publishes into typed structs | a publish could amplify memory and CPU in the proxy | small |
| 01c778b | N-73: send the host the egress policy allowed | domain fronting past the egress allowlist | small |
| 4ffcf4d, 47e1d9c | N-75: bound host-held header maps by the instance's memory limit | a guest could hold unbounded host memory in headers | small |
| 3071c8d | N-74: stamp worker telemetry at the wire level | a hostile 1 MiB batch cost 278 MB / 308 ms; now 3 MB / 10 ms | small |
| 0ac2849 | wasmtime 50.0.1 | security release (five advisories) | 0 |
| 6fdb057, 98f25be, 80b9fe6 | N-502..N-504: the journal refuses paths it can't name; recovery treats impossible paths as nothing there | **a client could brick an object for good** (503 on every later request) with a create under a file, a 256-byte name, or a path past PATH_MAX, followed by any failure. Found by a model-based test and a hostile guest | +34 |
| ea4019b | N-551: the worker checks its parent by the supervisor's pid | **in a container without an init (Kubernetes, `docker run` without `--init`) no worker could start** | +2 |
| 37dd88e | N-552: workers start from the supervisor's own image (`/proc/self/exe`) | replacing the binary under a running node started mismatched workers (502) | +2 |
| ac3c71f | N-507: the proxy refuses a TXT trust identity containing a comma | the node then refused every request for that host; found by fuzzing | 0 |
| 51d517e, 5a9fe61 | N-640: the CPU budget charges the thread's CPU clock, not wall time inside polls | **on a busy node, waiting for a core was billed as CPU**, so overload turned into CPU kills and restarts. Before: 178 of 1.4M GETs and 94 of 140k writes killed at load ~25; a request needing 33 ms of CPU was killed after 12–16 ms. After: none; the kill comes at 102–103 ms of real CPU for a 100 ms budget. Also fixes N-600 (SQLite's fsync waits were billed as CPU). Cost about 1.3 µs (~2%) per warm GET, within noise | +3 |

Plus the merge of `origin/main` 59b3783 (Amp's W-9 local capture), unchanged.

## 2. `cf/round3`: improvements on top

Pick in this order; each line says what it depends on.

### Faster app restarts and fairer cores (from `cf/speed`)

| Commit | Change | Evidence | Lines |
|---|---|---|---|
| bda2840 | H2: heavy requests yield their thread at every tick | a spinning guest can no longer hold a thread between ticks; neutral for ordinary requests (×1.00) | +9 |
| 091c810 | C4: the supervisor keeps one worker spawned ahead, engine already built | app restart (first request after its worker is gone) 21.5 → 11.9 ms | +54 |
| f23cda4 | C5: the worker opens its cached component file and compiles only on a miss | restart of a 5.6 MB component 31.2 → 8.9 ms. With C4: restart 19.9 → 10.2 ms, 5.6 MB 41.1 → 11.0 ms, LRU retirement 22.4 → 12.9 ms | +21 |
| 70df4d9, 285d96b | H3: the store re-polls on its own wakes; the worker exports telemetry from its own thread | HTTP through supervisor + jailed worker, one client: CPU per request 467 → 302 µs (×0.62), throughput +17–28%, worker context switches 4.7 → 1.1 per request. 256 clients: about ×0.94 CPU. Spans delivered 99.7–100%. Taken after N-640: with CPU billing, kills under load were 0 with and without H3 | +61 |

Not taken: H4 (link sockets registered for reads only, `cf/speed` ea938cf): a further ~10% one-client
CPU for +64 lines and two `unsafe` blocks. Optional.

The C4 and C5 commits were adapted to N-551 (the worker spec carries the supervisor's pid in its
second part). Two tests were adapted: the backup drill counts only jailed workers (C4 keeps a
spare), and `tests/dev.rs` reads the pulled component from its cache file (C5).

### Developer on-ramp and 22 example apps (`cf/onramp`, `ideas/examples`, merged at bac4d20)

- `nanovm-sdk` (Rust: one async function from request to response; SQLite, streaming, SSE,
  cookies, `after(work)`, outbound streaming), `tools/build-app.sh`, `nanovm run app.wasm` (local
  dev server with reload), `nanovm publish` (checks the app as a node will, pushes it).
- JavaScript apps on QuickJS with a Workers-style `fetch(request, env)`.
- `examples/hello`, `examples/chat`, and 22 more (URL shortener, rate limiter, locks with fencing
  tokens, polls, leaderboard, shop with all-or-nothing checkout, job queue, SSE, uploads with
  ranges, static site, presence, tic-tac-toe lobby, feature flags, wiki, notes with FTS5,
  analytics, reminders, offline-first todos, webhooks, LLM gateway, feeds, OAuth), each with an
  end-to-end test that checks eviction and restart. See `notes/examples.md`.
- Runtime: +171 lines (onramp) and +6 (N-602, `nanovm run` says when it lowers a limit).
  `oci-client` becomes part of the production binary (for `nanovm publish`).

### Many nodes and caller identity (`cf/multinode`, ported as d4e0c7d and 748e488)

- **W-10.** The proxy places each object on a node by rendezvous hashing and follows a node's 421
  to its owner. Nodes take per-object leases with rising epochs from an object store with
  conditional writes (S3, R2 or MinIO; in-memory for tests). A node that lost a lease can neither
  serve nor commit the object: the check sits at W-9's commit decision, and the captured version
  records the epoch. Calls between objects cross nodes through the proxy. With one node there is
  no store and no measurable cost (CPU per request 105.6 vs 102.0 µs, within noise).
  +473 runtime lines; `object_store` dependency (7 crates).
- **W-602.** An object can tell which object called it from `nanovm-callers`, which only nodes
  set: a client's forged header never reaches the guest, on one node or across nodes.
- Failover and late-commit tests pass; see `notes/multinode.md` for the safety argument and its
  limits (the epoch is fenced locally; shipping must be conditional on the lease record, W-90).

### Cheaper durable writes and committed state off the node (`cf/storage`, ported as 7c3d447..4ff1df6)

- **Fewer syncs per write.** The undo log is kept and a commit writes a new key that voids every
  record at once; records made back to back share one sync; commits run off the worker thread;
  a guest's own `sync` is a no-op because the commit already makes the request durable. A counter
  write costs 5 device flushes instead of 9 (with capture, 7 instead of 17).
- **Evidence on round3** (quiet machine, dev profile, 3 alternating rounds, medians): concurrent
  `POST /incr` 1,058/s → 1,978/s (1.87×), p99 1.06–1.28 s → 0.46–0.48 s, CPU per concurrent write
  1,121 → 870 µs (−22%). Serial writes are flat on a quiet disk (385 → 394/s) and cost about 12% more
  CPU, because each writing commit is handed to a blocking thread; under disk contention serial
  writes went 40 → 75/s.
- **Committed versions as changes (format 3).** A snapshot every 16 versions, changes in between
  (a 1-byte change to a 1 MiB file is a version under 8 KiB), each carrying the lease epoch.
- **Shipping and restore.** Versions are shipped in order to a put-once store (a directory, or S3,
  R2 or MinIO through `object_store`), only while this node holds the object's lease (W-90), and an
  object can be restored on a fresh root. The default acknowledgement stays local; `remote_ack`
  holds a commit up to 2 s for the store.
- **Limits.** A short window between reading the lease record and the put remains (W-90, needs a
  head pointer written with `If-Match`); the shipper polls every 20 ms (N-165); restore reads every
  version (N-166); a log left uncommitted by an older binary is refused, so drain before upgrading.
- +494 runtime lines, no new dependencies. All tests pass, including 114 SQLite recovery cuts and
  the new lease-fenced shipping tests.

## 3. Branches to pick on their own (no runtime change unless stated)

| Branch | What | Notes |
|---|---|---|
| `ideas/components` 7a3b8dd | queue, JSON documents (indexes, full-text, vectors), time series, templates, auth (JWT/JWKS, argon2id, sessions) as composed Wasm providers, like KV and SQLite | 0 runtime lines; 21 tests; squash f641461 into 1cac77b |
| `ideas/tooling` | `tools/inspect`, `tools/volume.py` export/import, `tools/doctor`, `tools/local-stack`, `tools/debug-run` and a debugging guide, `tools/replay.py` | +3 runtime lines (`app::import_allowed`) |
| `ideas/ops` | hardened systemd units, single-node reference deployment and host checks, container image, release workflow, benchmark history, errors guide, runbooks, filesystem matrix, Prometheus alerts | docs, scripts and workflows |
| `ideas/deploy-validation`, `ideas/seccomp-drift`, `ideas/supply-chain`, `ideas/fuzz`, `ideas/quota-model`, `ideas/hostile2-final`, `ideas/soak`, `ideas/mutants`, `ideas/quality` | validation tests, syscall drift check (no drift), cargo-deny/audit, six fuzz targets, a model-based quota test, a second hostile guest, a 30-minute soak (RSS and fds flat), tests for untested seccomp rules | tests and CI only |
| `cf/speed` ea938cf | H4, optional (above) | +64, two `unsafe` |

Research results, not adopted: pre-initializing Rust guests or the SQLite provider (Wizer) only
helps guests that build state at start, and it freezes std's hash keys into the snapshot so every
instance shares them (W-640). JavaScript keeps its pre-initialization.

## 4. Decisions for the owner

1. **W-9 acknowledgement and RPO**: still open. Shipping exists; the default stays durable on this
   node, with `remote_ack` as the opt-in.
2. **Multi-node**: lease length (recommend `lease_ms` 10 s, `failover_ms` 30 s); the lease store
   becomes a hard dependency (N-181); expiry by each node's own clock; keep `object_store`; calls
   go through the proxy when leases are on; cross-node cycles not along one chain end at the
   timeout (N-180).
3. **On-ramp**: `oci-client` in the production binary, +171 runtime lines.
4. **N-228**: every worker start syncs about 30 directories; cheaper but touches N-38's guarantee.
5. **W-600**: let a request roll back and still choose its status (a declined checkout can only
   answer 500 today). **W-601**: an app can't serve `/`.
6. **N-603**: the SQLite provider now allows the read-only pragmas FTS5 needs.
7. **N-505** (open): request bodies have no time limit at any hop.

## 5. Open items found this round

N-500 (the linker defines all of WASIp2; the import allowlist is the only guard), N-505, N-506,
N-670 (Landlock ABI 2 needed for cross-directory renames), N-671 (`ulimit -v` makes workers fail
without a clear message), W-621..W-623, W-90's remaining window, N-165 and N-166.
