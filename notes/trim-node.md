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
