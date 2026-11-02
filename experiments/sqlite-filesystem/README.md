# SQLite over nanovm's existing filesystem

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

### Original amplification and the generic fixes

On the original runtime, repeated autocommit updates to one row exhaust the
request journal even though the main database is only 32,768 bytes. The undo
log reaches 1,035,176 bytes, including 991,232 payload bytes covering only 8,192
distinct database bytes. The retained-file budget is actually reached first:
120 deleted SQLite journal files occupy 1,046,400 bytes, plus a 6-byte KV value.
Both budgets are 1 MiB. Propagating the error rolls back the entire request,
including earlier SQLite, file and KV writes.

In that baseline, if the guest swallows the error, closes SQLite and returns HTTP 200, reopening
preserves exactly the 120 completed statements, with no partial failed statement.
A second case performs 119 updates, then exhausts the journal during an attempted
multi-page update: all earlier updates survive, and none of the failed bulk
update appears. Both databases pass integrity checks and file/KV writes commit.
This matches the existing rule: returning a response commits, regardless of
status; returning an error or trapping rolls back. Catching an SQL error does
not mean the whole request should roll back automatically.

The exhausted connection returned `interrupted` on subsequent queries in an
exploratory run. Closing/reopening is tested; continued use of that connection
is not supported by this result. The VFS/error mapping needs production work.

The generic fix records each original byte range once per request and stops
retaining deleted files created during that request, while charging live unlinked
handles until their last owner closes. Those amplification fixes preserve the
undo format, quotas, reverse recovery, sync ordering and request commit policy. SQLite retains
its own rollback journal for statement/savepoint rollback inside a request.

The separate rename-failure correction restores staged targets and their quota
charges on both logging and syscall failure, preserves dirty-source sync tracking,
and cancels failed rename records so path reuse cannot corrupt recovery. It adds
journal tag 5; see the root README's downgrade restriction. Failed restoration,
cancellation or journal append I/O prevents commit until recovery. A temporary
real-WASI capability-denial probe changed from one `NotPermitted` followed by
nine spurious `InsufficientSpace` errors to ten `NotPermitted` errors, with exact
contents preserved through commit and rollback/restart. The probe was removed;
generic regressions retain coverage of accounting, path reuse, restoration
failure and interrupted recovery. Physical power-loss testing remains out of scope.

The new 1,000-update check fails on the original runtime. With both fixes it
passes in each mode, checking exact committed state and, separately, intentional
whole-request failure and recovery. Measurements below are taken after all
1,000 statements, before outer recovery:

| Mode | Undo log bytes | Retained-file bytes |
| --- | ---: | ---: |
| DELETE | 56,662 | 6 |
| TRUNCATE | 8,744 | 6 |
| PERSIST | 8,735 | 6 |
| MEMORY | 8,662 | 6 |
| WAL, exclusive, autocheckpoint=16 | 51,466 | 78,350 |

The 6 retained bytes are the original KV marker, which rollback must restore.
WAL additionally retains its request-start WAL files when the connection closes;
these are necessary for rollback, not accumulated intermediate journals.
DELETE still records repeated creation metadata: this is reduced amplification,
not an unbounded-work promise. Each budget remains capped. Tests also attempt
an oversized INSERT and multi-page UPDATE after 1,000 successful updates;
handled failures preserve exactly those earlier updates after reopening, and
propagated failure restores the complete request-start state. These replace
tests that expected the old 120-statement amplification failure.

The generic changes remain independent of SQLite. Their regressions cover
overlapping ranges, truncate/regrow, rename/recreate, interrupted recovery,
repeated KV replacement, and live unlinked handles. General host I/O failure
injection remains separate work.

## DELETE versus exclusive WAL benchmark

Run after building the fixture:

```sh
cargo test --locked -p nanovm --test integration benchmark_database_modes -- --ignored --nocapture --test-threads=1
```

This writes raw samples to `target/sqlite-filesystem/benchmark-results.json`.
[Recorded results](benchmark-results.json) are from 2026-11-02, an E2B orb on
Linux 6.1.158+ x86_64, ext4, Rust 1.99.1, WASI SDK 34 and the committed dependency
lockfile. The runtime is main's generic journal fixes through `40c5895`;
there are no SQLite-specific runtime modifications. These are single-client
orb measurements, not a production-storage throughput or concurrency claim.

Method: 1,024 rows with 512-byte payloads, FULL sync, 2 MiB SQLite page cache,
memory temporary storage, 16 MiB volume quota and 64 MiB guest memory. Initialization
is checkpointed with TRUNCATE before warmup so deferred creation work does not
favor either mode. Each case has three warmup requests and 25 measured requests,
repeated three times with mode order rotated: 108 fresh databases and 2,700 timed
requests. Compare DELETE with exclusive WAL at 16 and 1,000 checkpoint pages,
with a cached connection or one reopened/closed on every request. Compilation,
initialization, warmup and verification are outside timing. Request time includes
in-process Node dispatch and the outer journal commit, not TCP/HTTP transport.

The workloads are indexed point lookup, scanning all payload lengths, one
varying-row update, 100 varying-row updates in one SQL transaction, 100 separate
autocommit updates to the same row, and a 64 KiB blob overwrite that changes
every byte. The batched and autocommit cases intentionally exercise different
page locality; they are not a controlled comparison of SQL batching alone.

**Mean milliseconds per request, including final close/checkpoint amortized
over the 25 measured requests:**

| Connection | Workload | DELETE | WAL-16 | WAL-1000 |
| --- | --- | ---: | ---: | ---: |
| Cached | Point lookup | 0.24 | 0.22 | 0.24 |
| Cached | Length scan | 0.59 | 0.63 | 0.55 |
| Cached | One update | 5.46 | 3.13 | 3.06 |
| Cached | 100-update transaction | 14.76 | 24.88 | 7.04 |
| Cached | 100 autocommit updates | 206.58 | 41.29 | 103.87 |
| Cached | 64 KiB overwrite | 16.04 | 30.96 | 3.33 |
| Reopened | Point lookup | 0.63 | 2.47 | 2.47 |
| Reopened | Length scan | 1.82 | 4.09 | 4.12 |
| Reopened | One update | 5.60 | 5.62 | 5.56 |
| Reopened | 100-update transaction | 14.16 | 14.72 | 13.69 |
| Reopened | 100 autocommit updates | 207.75 | 33.44 | 64.55 |
| Reopened | 64 KiB overwrite | 15.21 | 14.48 | 15.37 |

Cached-connection request latency, **p50 / p95 milliseconds**, excluding the
separate final close (75 samples per cell, nearest-rank p95):

| Workload | DELETE | WAL-16 | WAL-1000 |
| --- | ---: | ---: | ---: |
| Point lookup | 0.21 / 0.42 | 0.14 / 0.24 | 0.13 / 0.26 |
| Length scan | 0.57 / 0.82 | 0.53 / 0.74 | 0.45 / 0.65 |
| One update | 5.27 / 7.16 | 2.29 / 4.13 | 2.23 / 3.03 |
| 100-update transaction | 13.99 / 21.41 | 23.73 / 39.66 | 2.68 / 3.45 |
| 100 autocommit updates | 200.92 / 275.33 | 39.67 / 54.59 | 108.15 / 142.96 |
| 64 KiB overwrite | 14.37 / 26.57 | 29.95 / 46.49 | 2.73 / 6.16 |

Final close for cached WAL-1000's batched updates costs 91–115 ms: ignoring it
would overstate the improvement. Warmup is excluded, but any remaining warmup
frames flushed by final close are included, conservatively charging WAL for them.
WAL-16 is fastest for repeated same-row autocommits, but slower than DELETE for
batched updates and large overwrites. WAL-1000 allows more deferred work and
larger files. Closing every request removes most of WAL's write advantage and
adds overhead even to reads.

For cached 100-autocommit requests, mean SQLite VFS write traffic and sync calls,
including amortized final close, are:

| Mode | KiB written/request | VFS sync calls/request |
| --- | ---: | ---: |
| DELETE | 1,652.73 | 300.00 |
| WAL-16 | 427.66 | 118.80 |
| WAL-1000 | 402.83 | 100.32 |

These are **SQLite's writes and sync calls, not total host writes/fsyncs**:
nanovm also persists undo records, directories and its outer commit. Fewer VFS
syncs do not guarantee less latency (WAL-1000 versus WAL-16 demonstrates that).
The largest observed database+WAL size between requests is 4,619.47 KiB for
WAL-1000; WAL-16 reaches 800.48 KiB in the blob case. This sampling excludes
within-request peaks, transient DELETE journals and the outer undo/retained
storage, so it is not a complete disk-budget bound. No host-fsync or CPU profile
was collected; the exact source of the checkpoint-policy latency difference is
not established by these measurements.

All 108 cases check response values, final totals and SQLite integrity outside
timing. Independent native SQLite validation additionally checked every final
row value and complete blob payload: 110,592 rows across 108 databases. The
six correctness tests also pass, including all 689 fault cuts and the existing
DELETE-mode worker SIGKILL test. No latency thresholds are asserted in CI;
the benchmark is deliberately separate from the `sqlite_` correctness filter.

**Decision:** use DELETE internally, with no app-facing journal controls.
WAL remains implementation research: neither tested checkpoint threshold is a
universal best setting. Do not weaken sync or outer request durability to obtain
the measured benefit: both modes used FULL throughout.

## Component composition proof

`provider/` contains SQLite and reuses the P3 VFS. `client/` contains only the
HTTP application and generated Spin imports, with no SQLite dependency. `wac plug`
connects them into `composed.wasm`. The uncomposed client is rejected by nanovm's
unchanged import allowlist; the composed artifact runs on the real Node runtime.
There is no host SQL handler or allowlist exception.

**The original 3.0 interface recommendation did not survive this test.** A
synchronous exported SQL call traps with `cannot block a synchronous task before
returning` when SQLite waits for P3 I/O. Forcing async bindings for those same sync
WIT functions is not valid either: composition rejects the async canonical option
on a synchronous function type. Use Spin's existing **3.1 async methods**, not a
modified 3.0 WIT. The vendored [upstream WIT](wit/deps/spin-sqlite/sqlite.wit) is
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

Read-only result overflow is catchable. Write RETURNING overflow traps: SQLite
may already have changed data, so the caller must not commit unseen writes.
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

## Revised interface proposal

**Keep SQLite in Wasm, backed by the existing filesystem. Reuse
`spin:sqlite/sqlite@3.1.0` unchanged, using its async methods.**
Do not add native SQLite storage calls to the host: they would bypass the P3
quota/journal interception demonstrated here. The nontrivial integration is a
P3 VFS, not teaching nanovm how SQLite commits.

There is no mature WASI SQLite standard to adopt: [WASI SQL](https://github.com/WebAssembly/wasi-sql)
is a Phase 1 proposal. [Spin's published SQLite WIT](wit/deps/spin-sqlite/sqlite.wit)
is an existing ecosystem interface, not a WASI standard. The supported entry points
are:

- `connection.open-async(database: string) -> result<connection, error>`
- `connection.execute-async(statement: string, parameters: list<value>)`
  returns column names, a row stream and a completion future, or an error.
- `connection.last-insert-rowid-async() -> s64`
- `connection.changes-async() -> u64`
- `value`: `integer(s64)`, `real(f64)`, `text(string)`, `blob(list<u8>)`, `null`.
- `query-result`: column names and rows, each row containing a list of values.
- Errors: `no-such-database`, `access-denied`, `invalid-connection`,
  `database-full`, `io(string)`.

Use the upstream WIT verbatim, not this prose as a new WIT definition. The
resource/open/typed-values shape fits config/keyvalue's capability interfaces.
Materialization can stay an implementation detail behind the 3.1 stream API.

[Cloudflare's SQLite-backed Durable Objects](https://developers.cloudflare.com/durable-objects/api/sqlite-storage-api/)
are the useful ownership model: a database belongs to one serialized object.
[D1's prepared statements and batch API](https://developers.cloudflare.com/d1/worker-api/)
are useful comparisons, but JavaScript cursors, batch methods, sessions and
replication controls need not become extra WIT surface here. Parameter binding
and returned rows already cover the basic use case.

### Small initial policy

1. `open-async("default")` selects this object's private database. Do not accept
   arbitrary filesystem paths or promise databases shared across objects.
   Other names return `no-such-database` until named databases are needed.
2. Exactly one live connection per provider instance/object. A second open returns
   `invalid-connection`; dropping it allows reopening. DELETE journaling, FULL sync
   and memory temporary storage are internal policy, with no app-facing controls.
   Multi-connection WAL is outside the intended scope, not a shipping prerequisite.
3. Execute one parameterized statement at a time, with the input/result bounds
   above. Ordinary SQL failures return the upstream error type. Storage failures
   and write-result overflow trap, forcing whole-request rollback and instance
   disposal rather than catchable errors on a potentially suspect connection.
4. Nanovm's response boundary remains the durable commit boundary for SQL,
   files and KV together. SQLite COMMIT alone is not a durable nanovm commit.
   Returning an error/trapping rolls all three back; an HTTP error status does not.
5. Initially reject explicit transaction/savepoint control, ATTACH, and changes
   to journaling/locking policy using SQLite's authorizer, not SQL text matching.
   These are implementation restrictions, not a claim of full Spin behavioral
   equivalence. The experiment shows completed SQL transactions and ATTACH can
   work; it does not solve transactions left open across a response. Broaden
   the policy only after specifying and testing request-boundary finalization.

This packaging now runs as two composed Wasm components. The proof establishes
the async/resource boundary, not full Spin behavioral equivalence or production
readiness. The synchronous 3.0 API is not supported over this P3 VFS.

### Permanent support, and what to do with key-value/config

Make the filesystem/journal the shared durability mechanism, not the only
application-facing API. Keep small WIT capabilities for apps. The smallest next
implementation is an optional composed provider using the upstream Spin SQLite
WIT, one owned connection and a production-quality P3 VFS. Its contract tests
must cover typed bindings/results, resource drop/reopen, multiple handles,
bounded errors/results, traps and response commit/rollback. Keep the current
SQL/file/KV recovery matrix against it. Extend the composed-provider tests rather
than adding a host SQLite service or a new runtime import.

The provider can cache its connection across successful requests, but a failed
request must discard the whole instance before filesystem recovery. Until there
is a request-finalization hook, do not leave SQL transactions open across responses;
initially deny transaction control as above. This costs some batching efficiency,
but avoids inventing an implicit commit protocol or pretending that the outer
journal understands SQLite's in-memory transaction state. Explicit transactions
and ATTACH can follow once their lifecycle is specified and tested.

WAL is not an app option in this proposal. If it is adopted internally later, reserve
disk headroom for database, WAL, undo and retained files; checkpoint thresholds
are triggers, not hard size caps. Document database-path ownership and refuse
unsupported concurrent opens; local no-op locks are not a production solution.
The first support tier can forbid raw writes to provider-owned files by contract;
hard isolation from the same app's raw filesystem access would need a separate
capability boundary and is not supplied by composition alone.

**Key-value already rides on this filesystem.** `crates/nanovm/src/kv.rs` stores
one base64url-named file per key under `/data/.kv/<bucket>`, using the same
QuotaGuard and Journal as raw filesystem operations. Keep `wasi:keyvalue` as the
public API. Moving its provider from host Rust into Wasm might eventually simplify
the host, but is a separate experiment: preserve atomics/batch/error semantics,
memory limits and mixed SQL/file/KV rollback, and measure the extra boundary
crossings before removing the existing provider. Do not move KV into SQLite
merely for implementation uniformity.

**Config has different inputs:** `crates/nanovm/src/app.rs` builds read-only, app-scoped
`WasiConfigVariables` from deployment configuration and shares it across objects.
It is not per-object mutable state. Keep `wasi:config` as the API; its implementation
could move to Wasm if deployment values can be supplied through a simpler existing
capability, such as a read-only mount. Evaluate that alongside KV in ISSUES.md W-17.
The host must still supply deployment inputs and enforce capabilities; moving
parsing/lookup code into Wasm does not eliminate that responsibility.

## Remaining gates, not established support

- In-flight async-call cancellation, resource/stream fault boundaries beyond the
  tested cancellation cleanup, and raw-file interference with a live database.
  Storage-error cache reuse is deliberately unsupported: those failures trap.
- Filesystem-backed temporary spill and unique temp names. This build forces
  memory temp storage and the placeholder name is not a general temp-file API.
- Power loss, host I/O failure, and crash injection inside nanovm commit/recovery.
  VFS cuts are guest traps after completed host calls; SIGKILL is process death,
  not storage power loss. Neither proves every internal ordering boundary.
- Total host I/O/CPU profiling, large result sets and workload comparisons against
  native SQLite/Spin/workerd. The measurements above compare modes within nanovm,
  not these products or production hardware.

No runtime changes were necessary for the initial workloads; the generic fixes
allow the repeated-update workload to fit its existing quota. The eight SQLite
tests are ignored by the ordinary suite because they need the separate fixture
and C toolchain; this branch's CI explicitly builds and runs them. This is an
experimental implementation and proposal, not a shipped interface or a claim of
full SQLite compatibility.
