# SQLite over nanovm's existing filesystem

For the packaged source beta and runnable app, start with
[components/sqlite](../../components/sqlite/README.md). This document retains the
experiments and design evidence; its broader SQLite modes are not the provider's
supported contract. The pressure fixtures use the packaged provider/VFS source.

An opt-in experiment, not a shipped SQLite interface. SQLite is compiled into
the guest's Wasm with rusqlite. The initial experiment needed no runtime changes;
this validation branch also includes the independent generic journal amplification
fixes. Quotas and the import allowlist are unchanged. This separate Cargo
workspace does not change or publish the normal guests.

## Reproduce

Install the Rust `wasm32-wasip2` target and unpack
[WASI SDK 34](https://github.com/WebAssembly/wasi-sdk/releases/tag/wasi-sdk-34).
From the repository root:

```sh
cargo install wac-cli --version 0.12.0 --locked
WASI_SDK_PATH=/path/to/wasi-sdk-34.0-x86_64-linux bash experiments/sqlite-filesystem/build.sh --compose
cargo test --locked -p nanovm --test integration sqlite_ -- --ignored --nocapture --test-threads=1
```

The script builds the original variants under `target/sqlite-filesystem/` and,
with `--compose`, the separate provider/client, `composed.wasm` and
`composed-faults.wasm`. Only the latter enables test-only SQL fault/scheduling
hooks; the normal artifact is tested to reject those functions:

- `stock.wasm`: libsqlite3-sys's bundled WASI VFS, using libc file operations.
  Nanovm rejects its `wasi:filesystem/types@0.2.12` import. Do not allowlist it:
  that would bypass the quota/journal wrapper on Preview 3.
- `p3.wasm`: a guest-side VFS using the existing `wasi:filesystem@0.3` imports.
  `wit_bindgen::block_on` bridges SQLite's synchronous callbacks to P3's async
  operations. `sqlite-vfs` supplies the C callback glue. No nanovm-specific
  storage API is used.

## What the tests exercise

Real SQLite with DELETE rollback journals and synchronous=FULL, bound INSERTs,
reads, a handled uniqueness violation, explicit SQL rollback, and a 256 KiB
blob followed by DELETE/VACUUM. The database grows from 12,288 to 274,432 bytes
and shrinks back to 12,288 bytes.

Each mixed write changes SQLite, a normal file, and a key-value entry. Guest
errors, traps, CPU exhaustion (both a guest loop and a recursive SQL query),
request timeout, and disk quota refusal leave the previously committed values.
An HTTP 507 response commits. Objects are isolated, and data survives eviction
and node restart. Every successful read checks `PRAGMA integrity_check`.

The second test uses the real HTTP server, a disposable loopback OCI registry,
and a jailed worker. It waits for the uncommitted file and KV writes on disk
(both occur after SQLite's INSERT completes), sends SIGKILL, and verifies that
the restarted worker returns only the previously committed SQL/file/KV values.
This tests the window after SQLite's commit and before nanovm's response commit.

The pressure matrix adds a second attached database, cache spilling, savepoints,
multi-page updates, and a connection cached across successful requests. It traps
after each successful VFS open, write, sync, truncate, or delete in the workload,
without unwinding or letting SQLite clean up. Every cut must recover the exact
original rows in both databases, file and KV markers, and pass both integrity
checks. Failed instances must discard their cached connection.

| SQLite journal mode | Independently exercised cuts |
| --- | ---: |
| DELETE | 135 |
| TRUNCATE | 143 |
| PERSIST | 143 |
| MEMORY | 62 |
| WAL, exclusive locking | 92 |
| Total transaction cuts | 575 |

Normal-locking WAL fails with a disk I/O error in this VFS. Exclusive-locking
WAL works without a VFS or host change; see below. MEMORY passes these
request-recovery checks because nanovm supplies
the durable outer journal; this is not a general endorsement of MEMORY mode.

### WAL works without shared memory under exclusive ownership

SQLite documents [WAL without shared memory](https://sqlite.org/wal.html#use_of_wal_without_shared_memory):
set `PRAGMA locking_mode=EXCLUSIVE` before the first WAL database access. The
fixture sets it immediately after opening the connection, before ATTACH, on
every fresh connection. SQLite maintains its WAL index in private memory and
does not call the VFS shared-memory methods. Database and `-wal` writes still
go through the unchanged P3 filesystem and nanovm journal. No `-shm` file is
created. Both attached databases report `journal_mode=wal`.

In addition to 92 mutation cuts, checkpoints pass with exact state recovery
after every successful VFS operation: PASSIVE 28 cuts, FULL 28, RESTART 28,
TRUNCATE 30. Both database checkpoints report no busy result and all frames
checkpointed; TRUNCATE leaves zero-length WAL files. These 114 checkpoint cuts
reopen fresh connections against recovered database/WAL files, rebuilding the
private index. Together with the earlier modes, this is 689 fault cuts.

**Checkpoint policy matters under small quotas.** SQLite's default automatic
checkpoint threshold cannot keep this workload within the 1 MiB object quota:
the main WAL reaches 1,027,982 bytes and the request fails; full SQL/file/KV
rollback succeeds. Setting `wal_autocheckpoint=16` lets 1,000 updates complete
and commit or roll back exactly. That is a tested fixture setting, not a proposed
universal threshold: it must fit the volume budget, and a single large transaction
can still run out of space before an automatic checkpoint occurs.

This is **single-connection WAL**, not general concurrent WAL. The fixture's
locks are still local bookkeeping; exclusive ownership is an assumption, not
enforced across multiple connections or raw-file writers. Normal-locking WAL
would need shared index/locking support. SQLite WAL alone does not make an
ATTACH transaction atomic across databases; nanovm's outer request journal is
what provides that boundary here. WAL-specific tests cover guest traps, not a
new jailed-worker SIGKILL or physical power-loss campaign.

## DELETE versus exclusive WAL

Decided 2026-11-02 from a single-client benchmark (108 databases, 2,700 timed
requests, FULL sync in both modes; the test and raw results are in git history):
use DELETE internally, with no app-facing journal controls. Exclusive WAL stays
implementation research; neither checkpoint threshold tested was best everywhere.

## Component composition proof

[The provider](../../components/sqlite/provider/) contains SQLite and the P3 VFS. `client/` contains only the
HTTP application and generated Spin imports, with no SQLite dependency. `wac plug`
connects them into `composed.wasm`. The uncomposed client is rejected by nanovm's
unchanged import allowlist; the composed artifact runs on the real Node runtime.
There is no host SQL handler or allowlist exception.

**The original 3.0 interface recommendation did not survive this test.** A
synchronous exported SQL call traps with `cannot block a synchronous task before
returning` when SQLite waits for P3 I/O. Forcing async bindings for those same sync
WIT functions is not valid either: composition rejects the async canonical option
on a synchronous function type. Use Spin's existing **3.1 async methods**, not a
modified 3.0 WIT. The vendored [upstream WIT](../../components/sqlite/wit/deps/spin-sqlite/sqlite.wit) is
byte-for-byte from [Spin commit 3f0ced5](https://github.com/spinframework/spin/blob/3f0ced5e2dcc1f9339f717f747b58c84d9afe3f0/wit/deps/spin-sqlite%403.1.0/sqlite.wit).

The provider uses `open-async` and `execute-async`; legacy synchronous open/execute
return an explicit unsupported-use error rather than trapping. The 3.1 result
stream and completion future cross the component boundary. For this proof,
SQLite fully executes and finalizes each statement before returning the stream;
rows are materialized in guest memory, so a slow consumer cannot keep a SQL
transaction open. This is not a large-result streaming implementation.

`sqlite_composed_provider` verifies all five typed value variants, column names,
41-row streaming, abandoned streams, errors followed by successful queries,
unknown database names, rejection of a second connection, cached reuse,
resource drop/reopen, policy denials, object isolation and restart. SQL/file/KV
changes roll back together on guest error, trap and quota refusal; HTTP 507 still
commits. The existing 689-cut matrix remains against the directly linked fixture.
The composed provider additionally passes 114 VFS cuts during multi-page UPDATE,
DELETE and INSERT, recovering exact original SQL/file/KV state and integrity.
Both artifacts now pass the jailed-worker SIGKILL campaign after SQLite commits
but before the response commits.

### Provider hardening and its accounting

Inputs are limited to 64 KiB SQL text, 128 parameters and 1 MiB of accounted
parameter values. SQLite additionally limits SQL length to 64 KiB, columns and
variables to 128, and individual values/encoded rows to 4 MiB. Results allow
4,096 rows per query and 1 MiB of aggregate accounted bytes across outstanding
streams, including streams from dropped connections. The accounting includes
column names, row/value structures and text/blob payloads before copying them;
completion or cancellation cleanup releases it. This is not an exact RSS cap:
allocation capacity, SQLite buffers and component-ABI copies remain subject to
the guest memory quota.

Read-only result overflow or invalid UTF-8 is catchable. Write RETURNING overflow
or result-conversion failure traps: SQLite may already have changed data, so the
caller must not commit unseen writes. SQLite TEXT can contain invalid UTF-8;
WIT strings cannot. Before the conversion fix, catching an error from
`INSERT ... RETURNING CAST(x'ff' AS TEXT)` committed the inserted row and earlier
SQL/file/KV writes. The regression now verifies exact request rollback, while
read-only invalid text, valid Unicode and binary blobs exercise the controls.
Storage errors (including quota exhaustion), interruption, corruption and other
uncertain SQLite failures also trap and roll back the whole request. Syntax,
constraint and policy errors remain catchable. This deliberately stricter
provider policy avoids reusing a suspect connection; it does not change the
host's response commit policy or the direct-linked experiment's behavior.

Boundary tests cover accepted/rejected input sizes and row counts, large results,
aggregate stream pressure, abandoned streams, and drop/reopen with an old stream
still charged. The result-bound regression fails against the original composition
proof. A test polls two async SQL calls concurrently while the first blocks for
10 ms inside SQLite: both complete, and the second observes that the first has
finished its blocking section. The current VFS uses blocking `waitable-set.wait`,
not an async yield permitting another call into the component. No new lock or
explicit concurrent-call rejection was needed. This invariant must be revisited
if execution starts yielding or cursors become lazy streams.

Interruption tests now kill a recursive SQL query on its CPU budget and time out
a test-injected host-clock wait inside a SQLite function. A filesystem marker
confirms the function was entered before timeout; after recovery the marker is
gone and SQL/file/KV state is exactly the prior committed state. These exercise
the real composed provider and Node, not mocked rollback. The clock wait is an
artificial blocked host operation, not an injected filesystem failure.

A caller-side cancellation probe was discarded: the execute future returned
ready on its first poll, so dropping it could not exercise mid-execution
cancellation. Request interruption and abandoned result streams are tested;
individual in-flight call cancellation is not established. Do not add an async
yield merely to make that test possible: it would change the serialization
assumption being tested.

## Unproven behavior outside that beta contract

- In-flight async-call cancellation, resource/stream fault boundaries beyond the
  tested cancellation cleanup, and raw-file interference with a live database.
  Storage-error cache reuse is deliberately unsupported: those failures trap.
- Filesystem-backed temporary spill and unique temp names. This build forces
  memory temp storage and the placeholder name is not a general temp-file API.
- Physical power loss, arbitrary host I/O failures beyond the tested boundaries,
  and further crash injection inside nanovm commit/recovery.
  VFS cuts are guest traps after completed host calls; SIGKILL is process death,
  not storage power loss. Neither proves every internal ordering boundary.
- Total host I/O/CPU profiling, large result sets and workload comparisons against
  native SQLite/Spin/workerd. The DELETE/WAL comparison measured modes within
  nanovm, not these products or production hardware.

No runtime changes were necessary for the initial workloads; the generic fixes
bound repeated-update overhead and preserve recovery data on failure. The eight SQLite
tests are ignored by the ordinary suite because they need the separate fixture
and C toolchain; this branch's CI explicitly builds and runs them. This is an
experimental implementation and proposal, not a shipped interface or a claim of
full SQLite compatibility.
