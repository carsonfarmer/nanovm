# Trim pass: the node's files

`trim/node`, from `beta/integration` 7988cae. Files: `supervisor.rs`, `telemetry.rs`,
`table.rs`, `main.rs`, `config.rs`, `app.rs`, `admin.rs`, `lib.rs` (2,421 runtime lines,
CI's count). `secrets.rs` and the secrets code in `supervisor.rs` (`start`'s secrets
lines, the watcher in `new`, `retire`'s caller, `secrets_file`) are left alone: secrets
are being redesigned next.

## Result

| File | Before | After |
|---|---|---|
| supervisor.rs | 974 | 934 |
| telemetry.rs | 356 | 338 |
| table.rs | 316 | 310 |
| main.rs | 238 | 233 |
| config.rs | 232 | 227 |
| app.rs | 212 | 215 |
| admin.rs | 70 | 64 |
| lib.rs | 23 | 23 |
| **Area** | **2,421** | **2,344** (−77, −3.2%) |

The runtime as a whole: 8,711 → 8,634. Targets: must reach 2,058, aim 1,816: **not
reached**. "What remains" below says, function by function, why.

**Whitespace check.** The tree isn't `rustfmt`-clean (formatting my files at 7988cae would
add 661 lines), so `cargo fmt` wasn't run: it would have rewritten every file and swamped
the delta. Instead both sides were formatted alone with the repository's `rustfmt.toml`
(`max_width = 120`) and counted the same way: 3,082 → 2,996 (−86). The saving is
logic, not layout.

## Inventory (before cutting)

### Duplication

| What | Where | Lines | Done |
|---|---|---|---|
| Take an app's running worker out, take its `max_apps` slot, drain it | `supervisor.rs`: `start`, `slot`, `held`, `retire` (through `drain`) | ~25 | `Worker::stop` (29349fc) |
| A server span with the request's method and path, truncated the same way | `telemetry.rs:Telemetry::forward`, `table.rs:Node::dispatch` | 6 | `Telemetry::request` (981d050) |
| Protobuf varints: decode, encode, encoded length | `telemetry.rs:varint`, `put_varint`, `nested`; `prost::encoding` has all three | ~22 | prost's (981d050) |
| The OTLP batch bound, checked again | `telemetry.rs:Client::send_bytes` (the exporter refuses first), `Relay::send` (`Limited` bounds the input) | 2 | removed (981d050, 08fd40a) |
| Record a refusal on the job's span, then answer | `table.rs:Node::enqueue` (3 times), `Node::dispatch` (call cycle) | ~8 | once, in `dispatch` (f1ca1d9) |
| Clamp an app's limits, then walk them again to say which were lowered | `config.rs:Limits::clamped`, `main.rs:run` | ~6 | `clamped` returns them (90b6023) |
| Check a component's imports and link it | `app.rs:App::load`, `main.rs:publish` | ~6 | `app::link` (90b6023) |
| Make, open and lock an app's storage | `supervisor.rs:app_dirs`, `app.rs:App::load` | 6 | `app::lock` (90b6023) |
| An answer carrying an egress verdict, its header overwritten for errors | `supervisor.rs:verdict_ok`, `call` | ~4 | `verdict(status, name)` (90b6023) |
| Write a value to a cgroup file, with context | `supervisor.rs:delegate`, `limit` (a closure each) | 3 | `write` (90b6023) |
| Validate the app and object an operator names | `supervisor.rs:restore` (twice), `delete` | 2 | `restore` as `delete` (90b6023) |
| Line framing on top of JSON for the worker's spec | `supervisor.rs:spawn`, `start` (newline), `worker_main` (line reads) | ~8 | one `Deserializer` (08fd40a) |
| Build an error reply | `admin.rs:serve` (parse error, command error) | 2 | one (d0dc399) |
| Give a tree to a uid | `supervisor.rs:chown_tree`; `ship.rs:own` (storage's) | 14 / 9 | for the coordinator (N-902) |
| An app directory's uid when root | `supervisor.rs:app_dirs`; `ship.rs:owner` (storage's) | 3 | for the coordinator (N-902) |
| Walk an app's objects through its shards | `app.rs:Objects::load`; `ship.rs:object_dirs` (storage's) | 6 | for the coordinator (N-902) |
| Lease, claim, release on failure, hold | `supervisor.rs:restore_object`; `lease.rs:serve` (storage's) | ~10 | for the coordinator (N-902) |
| The store's version keys parsed back into sequences | `supervisor.rs:command` (`versions`); `ship.rs:version_key` (storage's) | 3 | for the coordinator (N-902) |
| An egress verdict answer | `supervisor.rs:verdict`; `egress.rs:serve_forwarded` (edge's) | 4 | for the coordinator (N-902) |
| Record a refusal on the span and answer | `table.rs:Node::dispatch`; `object.rs:run` twice (edge's) | ~4 | for the coordinator (N-902) |
| A body that holds a guard; an empty answer | `supervisor.rs:forward`, `verdict`, `admin`; `object::holding`, `object::empty_response` (new on `trim/edge`, not in the base) | ~7 | after `trim/edge` merges |

### Other candidates, and what became of them

- **Generality:** `Supervisor::uids` (a lock threaded through two call sites into its one
  user: now a static in `app_dirs`); `cgroup` and `_cgroup_held` (one field); `Failed`
  copying the digest and policy out of its `AppRef` (keeps the `AppRef`); `AppConfig`'s six
  `#[serde(default)]` and `deny_unknown_fields`, left from apps in the node's config (it is
  only read from the spec the supervisor writes whole); `Supervisor::admin`, an async
  method taking the request for its path (a plain function); the per-request clone of the
  app's host to make a worker entry (cloned when the entry is made); `count_backlog`'s two
  hand-written upgrades (`?`); `restore_object`'s ship loop mapping each result to test it;
  the admin socket telling a missing stale socket from one it can't remove (bind says so).
- **Checks:** the two OTLP size re-checks; `restore`'s second name check;
  `floor_char_boundary(min(len))` (it clamps itself).
- **Kept, checked:** `Supervisor::new`'s `validate` (programmatic configs skip `load`),
  `preflight` (refuses at start, not in each worker), the supervisor's route check (keeps
  clients off the worker's `/_nanovm/evict`), `dispatch`'s object check (calls from guests
  aren't routed), `Cgroup::take`'s magic and lock (N-700, N-702), `limit`'s
  `memory.current` check (N-704), `remove`'s storage lock (Q87), the relay's method check.

## Configuration keys

None removed. Every key of `NodeConfig`, `ProxyConfig`, `ClientLimits`, `CaptureConfig`,
`WorkerLimits` and `Limits` was checked for an owner decision or a test of a real need:

- `Limits` is the apps' manifest format (`nanovm.json`, `deny_unknown_fields`): none can go
  without breaking manifests.
- Tested or decided: `listen`, `data_dir`, `capture` (`max_mb`, `snapshot_every`, `store`,
  `remote_ack`: B-501, tests), `max_live_objects`, `max_objects_per_app` (B-202), `proxy`,
  `leases`, `name`, `lease_ms` (tests), `telemetry_endpoint` (Q58), `memory_reservation_mb`
  (tests), `epoch_tick_ms` (tests set 1), `jail_uid_base` (the ops reference configs set it;
  their runbook says never change it), `worker` (B-303; `pids` set by a cgroup test),
  `max_component_mb`, `load_timeout_ms`, `max_apps` (N-4, N-22, tests), `dev`, `secrets`,
  `cgroup` (B-303), `clients` (B-301; the edge agent removes `rate` and `app_rate`),
  `app_disk_mb`, `disk_reserve_mb` (B-302); the proxy's keys (Q54, Q55, B-400s, tests).
- **Candidates for the owner** (no decision, no test of a need; each is one line): 
  `heavy_lanes` (PLAN documents it; only a default is asserted) and `front_threads` (a unit
  test of its arithmetic; earlier benchmarks set it to 1). Kept: one line each is not worth
  a veto round, and the benchmark agent may use `front_threads`.

## What remains, and why

Each function was read for repeated logic, needless generality and checks another layer
makes; what is left is the behaviour, its bounds and its tests. The large pieces:

- `supervisor.rs` (934):
  - `start` (~120): fetch and authorize, the app's spec, the slot, the app's uid, the spare
    or a fresh worker, the reaper (killed after `post_return_timeout_ms` + 2 s, N-705), the
    cgroup limits, the stderr forwarder (its last line explains a failed start), the egress
    link (network, publishes, telemetry relay with 3 slots, with leases the lease and call
    routes), readiness.
  - `new` (~98): construction, then four background jobs, each a decided behaviour: the
    admin socket (B-208), shipping and the backlog count (B-500, B-501), lease renewal
    (W-10), secrets watching (left for the redesign); the snapshot's fields are what the
    collector-reading tests and the demo consume (Q62).
  - Operator paths: `restore` and `restore_object` (B-500: ship first, lease, restore, ship
    after), `held` (one hold for both), `pause`, `delete` and `remove` (B-205: drain the
    whole app, store first, rename into `deleted/`, so a crash leaves it whole or gone),
    `command` (six commands, each tested).
  - Worker plumbing: `ensure` (single-flight, the start backoff, `MAX_POLICIES`), `slot`
    (LRU eviction of an idle app, N-22), `spawn` (the cgroup entered before exec, fds 3 and 4),
    `Cgroup` (N-700 to N-702), `delegate` and `limit` (B-303, N-704), `app_dirs` and
    `chown_tree` (uids, Q87), `worker_main`.
- `telemetry.rs` (338): the SDK setup with its bounds (Q60-Q66), the export thread
  (N-222), the relay (N-74's wire-level stamping: the rest of the file).
- `table.rs` (310): routing, the actor table, call-cycle checks, leases, eviction, the
  worker's snapshot.
- `main.rs` (233): the five commands; `nanovm run`'s console, sockets and restarts.
- `config.rs` (227): the keys above, the lease-store and `remote_ack` rules (tested).
- `app.rs` (215): the import allowlist and linker (N-500), the request-head bounds (N-75,
  N-691), the object cap and room checks (B-202, B-204), `settle` (B-201).
- `admin.rs` (64): the socket in a 0700 directory reached through `/proc/self/fd` (B-508).

Reaching 2,058 would mean removing behaviour: for instance `nanovm run`'s console or
socket support, the supervisor's per-worker snapshot, the restore path's ship-first rule,
or the test-only `flush_telemetry` route. Those are owner decisions, not trims.

## Second pass: what each feature costs

The coordinator asked for a price on every feature in the node's files, so the owner can
choose what to drop. Lines are CI's count, at 3c9346c, in the node's files. A line shared
by two features is counted in both. "Elsewhere" is code in other agents' files that goes
with the feature, counted the same way. Each feature's lines were picked out by function
and line range (`tmp/prices.py` in the worktree; not committed).

### Features and behaviours

| Feature | Node lines | Elsewhere | What users lose without it |
|---|---|---|---|
| Multi-node leases: lease renewal, a restore under a lease, the worker's lease and cross-node call routes, `call`, `Node::acquire`/`release`/`renewed`/`misdirected`, the `leases`/`name`/`lease_ms` keys and their load and validate rules | 121 | lease.rs 271; parts of ship.rs, object.rs, egress.rs, proxy.rs | Several nodes, failover of an object to another node (W-10). One node only. |
| Worker cgroups at all: `Cgroup`, `cgroup`, `delegate`, `write`, `limit`, the spawn and start parts, the `cgroup` and `[worker]` keys | 109 | none | `[worker]` memory, CPU and pids limits per app worker (B-303), and cleanup of worker cgroups (N-700 to N-702). One app can use all the host's memory and cores. |
| – of which cgroup `"auto"` (`delegate` and its two `match` arms) | 22 | none | The default that works under systemd `Delegate=yes`. The operator must make a delegated subtree, enable cpu, memory and pids for its children, and keep the supervisor out of it (cgroup v2's no-internal-process rule), e.g. in an `ExecStartPre` script, then set `cgroup = "<dir>"`. Otherwise the default becomes `"off"`. |
| Live restore (`restore`, `restore_object`, the command arm, `held`'s object path) | 93 | ship.rs 150 (`restore`, `install`, `recover`, `pause`/`resume`/`paused`, staging) | `nanovm admin restore` on a running node, of one object or an app, to the head or an older version (B-500). A restore would need the node stopped and manual work. |
| The admin socket (`admin.rs`, `Supervisor::command`'s dispatch, `nanovm admin`) | 77 | none | Every operator command below, and the one control path into a running node (B-208). |
| `delete` / `delete-app` (`delete`, `remove`, the startup sweep of `deleted/`, `held`'s whole-app path) | 41 | ship.rs `Store::remove` (3 impls) | Erasing an object's or an app's data, in the store too (B-205; erasure requests). By hand it means stopping the node. |
| Backlog: `admin backlog`, `count_backlog` (its log lines), the three `nanovm.capture.waiting.*` snapshot fields | 30 | ship.rs 19 | Seeing what waits to be shipped during a store outage (B-501). |
| `pause` (public; `held`'s object path, and one test) | 14 | ship.rs (in the 150 above) | Only restore uses it. |
| `admin versions` | 10 | none | Listing an object's versions in the store without the store's own tools. |
| `admin apps` | 4 | none | Seeing which apps run, at which digest and pid. |
| Shipper hold (`held`'s `shipping` set) | 6 | ship.rs ~5 (`Held`, `run`'s check) | A delete or restore could race the shipper on the same object (the integration's `the_shipper_passes_what_is_held_over`). |
| Telemetry: the SDK export, spans, logs and snapshots (`telemetry.rs` up to `Relay`) | 214 | metrics.rs, object.rs, egress.rs calls | All observability: request spans, guest logs, snapshots (Q43, W-2, Q58-Q71). |
| Telemetry relay (`Relay`, the worker's `/telemetry/` route with 3 slots) | 46 | none | Every trace, log and snapshot of a jailed worker: it has no network of its own. |
| Telemetry stamping: each resource replaced by the supervisor's (wire level, N-74) | 41 | none | Tenant identity in exported telemetry. A worker could claim another app or version. See "Telemetry" below for a cheaper design. |
| – plus scrubbing forged identity attributes from scopes, spans, events, links and log records | 17 | none | A compromised worker could put `nanovm.app` or `service.*` on its spans and logs, though its resource is stamped. Tested by `relay_stamps_identity_and_bounds_worker_requests`. |
| Worker snapshots (`Node::with_engine`'s callback) | 28 | none | Per-app records of live objects, phases, poisons and counters. The tests' stats harness reads them. |
| Supervisor snapshots (process; per worker: up, pid, digest, version, policies, starts, link errors) | 23 | none | As above, for workers. The tests' harness pairs them with the app records. |
| `flush_telemetry` (used only by tests) | 13 | server.rs 6 (`/_nanovm/flush`) | Nothing user-visible. Tests would wait for the periodic (1 s) export instead of forcing one, so they get slower and need a new way to know a snapshot is fresh. |
| Spare worker (`spare`, `refill`, `Spawned`, `start`'s take, `live`'s refill) | 31 | none | Every first request to an app and every version switch would pay a process spawn plus an engine build (the pool alone took about 7 ms; N-224). |
| Start backoff (`Failed`, the 1 s check, recording the failure) | ~22 net | none | A broken app is started again on every request (a registry fetch and a spawn each) instead of once a second. |
| Policy check of a running version (`MAX_POLICIES`) | 10 | registry.rs `authorize` | A request naming a new trust policy for a running version would restart the worker rather than check its signature (N-17, N-26). |
| A switch finishes without its request (`live` runs `ensure` on the control runtime) | 10 | none | A version switch cancelled halfway when its requests go away (`a_switch_finishes_when_its_request_goes_away`). |
| LRU eviction of an idle app for its slot (`slot`) | 20 | none | At `max_apps`, a new app is refused rather than taking the least recently used idle app's slot (N-22). |
| `Worker::stop` (the one drain) | 12 | none | Required by any version switch. Not optional. |
| Forgetting app entries past 2 × `max_apps` | 3 | none | The worker map grows with every app name ever asked for. |
| Worker stderr forwarding and last words | 16 | none | Workers' stderr (guest panics, jail errors) and the reason a start failed. |
| Bounded registry fetches (`fetch`, `MAX_FETCHES`, `load_timeout_ms`) | 12 | none | The memory and time bounds on fetches (N-4, N-22). |
| Per-app uids (`app_dirs`' uid, `chown_tree`, `jail_uid_base`) | 32 | jail.rs | Workers of all apps run as one uid. A jail escape then reaches every app's data. |
| Secrets: the file watcher and restart (`new`'s watcher, `retire`, `secrets_file`, the stamp) | 26 | none | A changed secrets file takes effect at the app's next worker start, not within about 1 s. Goes with the move to identity tokens. |
| – all secrets code in the node's files (watcher, load in `start`, telemetry masking, `nanovm run --secrets`, the key) | 43 | secrets.rs 80 | Operator secrets for apps. Being replaced. |
| Call-cycle checks (`wait_for`, `reaches`, callers, `enqueue`'s check) | 29 | egress.rs | A call that closes a cycle waits for its request timeout instead of an immediate 508 (Q45). |
| WebSocket publishes from workers (egress `/publish`) | 8 | object.rs, ws.rs, proxy.rs | Publishes from jailed apps. |
| `nanovm run`: WebSockets in-process (`local`, `event`, `Node::sockets`, `enqueue`'s event path) | 26 | ws.rs | Sockets under `nanovm run`. |
| `nanovm run`: the console (`print`) | 38 | none | A line per request and the guest's output on the terminal. |
| `nanovm run`: restart on change | 19 | none | Restarting by hand after a rebuild. |
| `nanovm run`/tests: in-process shipping | 7 | none | Shipping without a supervisor. |
| `nanovm publish` | 37 | registry.rs `push_app` | Publishing with another OCI tool (oras, crane), then signing as now. |
| Request-head bounds (`bound_request_heads`) | 41 | none | N-75, N-691: a guest could hold many times its memory limit in request heads. |
| Object cap and disk room (`Objects`) | 62 | object.rs, journal.rs | B-202, B-204: no per-app object cap, and new objects on a full disk. |
| `heavy_lanes` key | 1 | none | Tuning the heavy lanes; fixed at half the cores. |
| `front_threads` key | 1 | none | Tuning front-end threads; fixed at a quarter of the cores. Benchmarks that pin it change. |

Some sums for the owner:
- The dispensable-looking group comes to about 133 lines (2,344 → about 2,211). That is the spare (31), the start backoff (22), cgroup `"auto"` (22), the secrets watcher (26), the attribute scrub (17), `flush_telemetry` (13, with a test-harness change), and the two keys (2).
- Adding multi-node leases (121) gives about 2,090.
- 2,058 then needs about 32 more, e.g. backlog and apps (34), live restore (93), the admin socket (77), or cgroups beyond `"auto"` (87).

### Configuration keys

Each key costs one line in `config.rs`, plus the node lines that exist only for it:
- `NodeConfig`:
  - `listen` 1; `data_dir` 1 (core).
  - `capture` 1, plus `CaptureConfig` 6, plus `load`'s `remote_ack` default 5 and `validate`'s 2 checks 6 (shipping, B-500, B-501).
  - `max_live_objects` 1 (core); `max_objects_per_app` 1, plus `Objects`' cap about 12.
  - `proxy` 1 (publishes, calls).
  - `leases`, `name`, `lease_ms` 3, plus about 15 of rules (multi-node row).
  - `telemetry_endpoint` 1; `memory_reservation_mb` 1; `epoch_tick_ms` 1 (tests set it).
  - `heavy_lanes` 1; `front_threads` 1; `jail_uid_base` 1 (the ops configs set it).
  - `limits` 1 (the manifest's caps); `worker` 1, plus `WorkerLimits` 5 and `host_memory_mb` 3 (cgroups row).
  - `max_component_mb` 1; `max_apps` 1, plus `slot` 20 and forgetting 3; `load_timeout_ms` 1, plus 2.
  - `dev` 1, plus `dev_on_loopback` 10; `secrets` 1 (secrets row).
  - `cgroup` 1 (cgroups row); `clients` 1 (edge's).
  - `app_disk_mb` 1; `disk_reserve_mb` 1 (quota.rs).
- `ProxyConfig` (one line each; their code is in proxy.rs, edge's):
  - `listen`, `nodes` (+3 in `validate`), `failover_ms` (+3), `publish_listen`, `telemetry_endpoint`;
  - the six socket caps (Q55);
  - `resolver`, `tag_ttl_s` (tests set 0), `dev`, `hosts` (+4), `signers` (+5), `clients`.
- `ClientLimits` (edge's): `rate` and `app_rate` (being removed), `connections_per_client`, `connections`, `trusted`.
- `CaptureConfig`: `max_mb`, `snapshot_every` (tests), `store`, `remote_ack` (+11 of rules).
- `WorkerLimits`: `memory_mb` (+5 in `limit`, the N-704 check), `cpus` (+3), `pids` (+3; a VM test sets it).
- `Limits`: the apps' manifest format (`nanovm.json`); removing any breaks manifests.

### Structural review (supervisor.rs, telemetry.rs, table.rs)

Each design question was checked against keeping every behaviour and test. None of the
redesigns below saves lines without dropping something, so none was implemented in this
pass. The numbers are drafts measured against this tree.

- **Telemetry: could the relay become the SDK exporter plus a much smaller stamp?** The
  SDK exporter is already what both ends use: the worker's exporter posts through its
  egress link, and the supervisor's relay is the only hop with network. What remains is
  the stamp (41) and the scrub (17).
  - A resource-only stamp: walk the batch once, replace each resource, copy the rest. It
    comes to about 20 lines (−38), but drops the scrub (a behaviour, and assertions in
    `relay_stamps_identity_and_bounds_worker_requests` and
    `stamping_matches_decoding_and_scrubbing`).
  - Stamping in the Collector: the relay forwards bytes with an `nanovm-app` header, and
    the Collector's `resource` processor upserts from request metadata (`include_metadata`).
    That removes the stamp and the scrub (−58), but moves a security property (Q58: "only
    the supervisor stamps identity") into deployment configuration.
  - Decoding with prost was rejected by N-74 (200× the batch's size).
  - Owner decisions, not trims.
- **Worker lifecycle: do the states collapse?** After the first pass the states are:
  - no worker;
  - starting (single-flight, holding the last failure);
  - live;
  - stopping (`kill` sent);
  - gone (`kill` closed).

  Each transition has one path, `Worker::stop`, except two that pick which worker to stop:
  `retire` (the secrets watcher) and `slot` (another app's LRU worker). Collapsing further
  means dropping the backoff state (start backoff row) or the spare (spare row).
- **Admin protocol: does it need its own framing?** It is one line of JSON in and one out,
  three lines on each side. Serving HTTP on the socket (`server::serve_conn` already takes
  any stream) would need a client too, a hyper handshake over a `UnixStream`. A draft came
  to about 10 more lines; `nanovm admin` would also need curl or that client.
- **The worker's egress handler as a struct** (`Egress { sup: Weak<Supervisor>, policy, app,
  host, digest, exports }` with a `serve` method, instead of a closure cloning 11 values):
  - The draft is +5 to +10 lines.
  - It would save 5 string and path clones per egress request.
  - It is a performance option, not a trim.
- **table.rs:** objects leave the table four ways: `evict` (restore), `evict_lru`
  (capacity), `try_retire` (the actor) and `renewed` (a lost session). Each is one
  `remove`, under a different trigger. Snapshots and leases are priced above. The rest is
  dispatch and the cycle check.

## Tests

Each commit's message lists the tests it ran. The full suite on the final code (d0dc399),
as root on the shared machine, debug build, `env -u RUST_BACKTRACE RUST_TEST_THREADS=1
cargo test --locked -p nanovm`: lib 194 passed (1 ignored), configs 1, dev 11, examples
29, integration 128 (14 ignored), js 17, committed_state 2: the same counts as the base
(`notes/beta-integration.md`).

Mutation checks, each restored with an empty `git diff` of the source:

- a wrong length in `telemetry::nested` fails both stamping tests;
- `Worker::stop` not waiting for requests to finish fails
  `a_drain_that_runs_out_of_time_kills_the_old_worker`;
- `Limits::clamped` reporting nothing lowered fails the changed
  `clamped_caps_each_number_and_keeps_egress`;
- the receiving node's call-cycle check disabled fails nothing, at the base as well:
  N-901.

The worker spec's new framing was checked by hand as well: `nanovm worker` given only the
node's config builds its engine (422 mappings at 64 slots) and then waits in `read(0)`
for its app, as a spare must (the test that checks this skips without a v2 memory
controller, as here).
