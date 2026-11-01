# SQLite over nanovm's existing filesystem

An opt-in experiment, not a shipped SQLite interface. SQLite is compiled into
the guest's Wasm with rusqlite; nanovm's runtime, journal, quotas, and import
allowlist are unchanged. This separate Cargo workspace does not change or
publish the normal guests.

## Reproduce

Install the Rust `wasm32-wasip2` target and unpack
[WASI SDK 34](https://github.com/WebAssembly/wasi-sdk/releases/tag/wasi-sdk-34).
From the repository root:

```sh
WASI_SDK_PATH=/path/to/wasi-sdk-34.0-x86_64-linux bash experiments/sqlite-filesystem/build.sh
cargo test --locked -p nanovm --test integration sqlite_ -- --ignored --nocapture --test-threads=1
```

The script builds both variants under `target/sqlite-filesystem/`:

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
| Total | 483 |

WAL fails with a disk I/O error in this VFS; the test verifies recovery, not WAL
support. MEMORY passes these request-recovery checks because nanovm supplies
the durable outer journal; this is not a general endorsement of MEMORY mode.

### Journal capacity is a real limitation

Repeated autocommit updates to one row exhaust the 1 MiB request journal even
though the main database is only 32,768 bytes. The measured undo log reaches
1,035,176 bytes. Propagating the error rolls back the entire request, including
earlier SQLite, file and KV writes.

If the guest swallows the error, closes SQLite and returns HTTP 200, reopening
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

The journal records overwritten bytes repeatedly, not just their first value
in the request, and SQLite maintains its own rollback journal too. This is an
amplification problem worth addressing, but these tests do not justify replacing
the journal or adding a SQLite-specific commit path. Benchmark bytes and syncs
before choosing an optimization. A possible generic improvement is recording
each original byte range once per request; that must still correctly handle
overlapping writes, truncate/regrow, rename/unlink/recreate and open descriptors.
Simply increasing the cap hides the problem, while disabling SQLite's journal
would lose statement/savepoint rollback inside a request.

## Revised interface proposal

**Keep SQLite in Wasm, backed by the existing filesystem. Reuse
`spin:sqlite/sqlite@3.0.0` unchanged as the candidate public interface.**
Do not add native SQLite storage calls to the host: they would bypass the P3
quota/journal interception demonstrated here. The nontrivial integration is a
P3 VFS, not teaching nanovm how SQLite commits.

There is no mature WASI SQLite standard to adopt: [WASI SQL](https://github.com/WebAssembly/wasi-sql)
is a Phase 1 proposal. [Spin's published SQLite WIT](https://github.com/spinframework/spin/blob/main/wit/deps/spin-sqlite%403.0.0/sqlite.wit)
is an existing ecosystem interface, not a WASI standard. Its complete API is
small enough; inventing `nanovm:sqlite` would add little:

- `connection.open(database: string) -> result<connection, error>`
- `connection.execute(statement: string, parameters: list<value>) -> result<query-result, error>`
- `connection.last-insert-rowid() -> s64`
- `connection.changes() -> u64`
- `value`: `integer(s64)`, `real(f64)`, `text(string)`, `blob(list<u8>)`, `null`.
- `query-result`: column names and rows, each row containing a list of values.
- Errors: `no-such-database`, `access-denied`, `invalid-connection`,
  `database-full`, `io(string)`.

Use the upstream WIT verbatim when implementing, not this prose as a new WIT
definition. The resource/open/typed-values shape fits config/keyvalue's small
capability interfaces. Prefer 3.0's materialized results over 3.1's async/stream
extensions for the first implementation; results remain bounded by guest memory.

[Cloudflare's SQLite-backed Durable Objects](https://developers.cloudflare.com/durable-objects/api/sqlite-storage-api/)
are the useful ownership model: a database belongs to one serialized object.
[D1's prepared statements and batch API](https://developers.cloudflare.com/d1/worker-api/)
are useful comparisons, but JavaScript cursors, batch methods, sessions and
replication controls need not become extra WIT surface here. Parameter binding
and returned rows already cover the basic use case.

### Small initial policy

1. `open("default")` selects this object's private database. Do not accept
   arbitrary filesystem paths or promise databases shared across objects.
   Other names return `no-such-database` until named databases are needed.
2. Start with one connection per database, DELETE journaling, FULL sync and
   in-memory temporary storage. Multiple resource handles must not silently
   create independent connections with this fixture's no-op locking.
3. Execute one parameterized statement at a time. Ordinary SQL failures return
   the upstream error type; journal exhaustion must not leak the fixture's
   misleading `interrupted` classification as the intended API contract.
4. Nanovm's response boundary remains the durable commit boundary for SQL,
   files and KV together. SQLite COMMIT alone is not a durable nanovm commit.
   Returning an error/trapping rolls all three back; an HTTP error status does not.
5. Initially reject explicit transaction/savepoint control, ATTACH, and changes
   to journaling/locking policy using SQLite's authorizer, not SQL text matching.
   These are implementation restrictions, not a claim of full Spin behavioral
   equivalence. The experiment shows completed SQL transactions and ATTACH can
   work; it does not solve transactions left open across a response. Broaden
   the policy only after specifying and testing request-boundary finalization.

The intended packaging is a reusable SQLite Wasm component exporting the Spin
interface and importing P3 filesystem, composed with an application. **This
branch has not proved component composition:** the engine is compiled directly
into an HTTP fixture. Prove composition and resource/async behavior before
committing to that packaging; no new host import allowlist entry is needed if
composition resolves the Spin interface internally.

## Remaining gates, not established support

- WAL shared-memory/checkpoint behavior and real multi-connection locks. The
  fixture only records locks locally; serialized requests alone do not solve
  multiple connections in one request.
- Transactions left open across responses, successful cache reuse after storage
  errors, and preventing raw-file interference with a live database.
- Filesystem-backed temporary spill and unique temp names. This build forces
  memory temp storage and the placeholder name is not a general temp-file API.
- Power loss, host I/O failure, and crash injection inside nanovm commit/recovery.
  VFS cuts are guest traps after completed host calls; SIGKILL is process death,
  not storage power loss. Neither proves every internal ordering boundary.
- Latency, CPU, journal bytes/sync amplification, large result sets and workload
  comparisons against native SQLite/Spin/workerd.

No runtime changes were necessary for the tested workloads. The four SQLite
tests are ignored by the ordinary suite because they need the separate fixture
and C toolchain; this branch's CI explicitly builds and runs them. This is an
experimental implementation and proposal, not a shipped interface or a claim of
full SQLite compatibility.
