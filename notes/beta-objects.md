# Beta: objects (`beta/objects`)

The production audit found that any anonymous client could create unlimited persistent
objects: every first path segment made a volume, nothing capped an app's objects, nothing
removed one, and there was no delete. This branch makes storage follow data, caps it, and
gives an operator one control path to delete. ISSUES B-201 to B-208; README "Data
lifecycle" and "Operator commands"; PLAN roadmap item 5.

Measured on one shared 4-core VM under heavy load from other agents (load average 10 to
26). Tests run serially (`RUST_TEST_THREADS=1`).

## What an object is on disk now

`objects/<app>/<shard>/<object>/`, where `<shard>` is the first byte of the id's SHA-256 in
hex. In it: `data` (the volume), `journal` (the undo log and the files a request removed),
and with capture `committed` and `shipped`. Before, the volume was `objects/<app>/<object>`
and the rest sat in `~journal/<object>`, `~committed/<object>`, `~shipped/<object>`: four
directories per object, three of them with one subdirectory per object of the app. An
existing data directory is not read: AGENTS.md rule 1 (start fresh).

## Root cause of the 300-GET scan

`object::new_store` made the volume before instantiating, for any valid id. The journal log
and the capture directory were already made only by a change (`open_log`,
`committed::prepare`); the volume was the one thing made up front, and nothing ever removed
it. So a 404 left a directory.

The fix gives an instance of an object that has no storage an empty directory from a
per-app pool (`objects/<app>/~new`, reused like `/tmp`'s scratch) as `/data`. The first
create in it (`open_at` with `CREATE`, `create_directory_at`) moves it into the object's
place (`Journal::place`), after taking a place under the app's cap. Nothing else can be the
first change of an empty volume. When the instance ends (the journal's `Drop`, which runs
after any commit in flight: the commit thread now drops its handle before it gives the
volume back), an object the instance made that never committed is rolled back at once (any
other object's uncommitted work still rolls back when it next starts, as the SQLite recovery
tests expect), and an object whose volume is then empty with no captured history is removed
(`Objects::settle`).

| Test | Before | After |
|---|---|---|
| `objects_that_change_nothing_leave_nothing`: 300 `GET /scanN/wp-login.php` (404) and 20 `GET /readN/` (200, count 0) on `counter` | FAILED: 320 object directories while the instances were live | 0 directories, live and after shutdown; one `POST /w/incr` makes only `w`'s |
| `failed_and_emptied_objects_leave_nothing` (`hostile`) | (new behavior) | a new object whose first request panics or errors after writing leaves nothing; one that answers (507) keeps its write; one that removes its files goes when its instance ends; one with a file left stays |

The pool holds at most one directory per instance live at once (the test allows 321 for
321 objects; they're reused).

## Per-app cap and N-35

`max_objects_per_app` (node config, default 10,000). The worker counts its app's object
directories when it starts (one `read_dir` per shard), checks the count before an object
without storage runs (507 `object-limit`, no guest code), and takes the place atomically at
the first create (`try_update`). A request to an existing object pays nothing: its volume
exists, so neither check runs.

- `an_app_has_at_most_its_objects`: cap 5, 40 clients at once each create a file in a new
  object: exactly 5 directories; a read of another unknown object gets 507
  `object-limit` naming `max_objects_per_app`; existing objects are served; one that empties
  itself and ends makes room for exactly one more; a node restarted on the same data counts
  5 again.
- `room_for_objects` (unit): 16 threads admitting at once against 5 free places: 5.
- N-35: 256 shards. `objects_are_sharded`: 65,000 ids (`user-0` …) put 217 to 287 in a
  shard (the test allows under 400), so the cap can go far past a filesystem's 65,000
  subdirectories.
  A client that crafts ids for one shard (easy: the hash is public) fills that shard only
  until the cap; on a filesystem without `dir_nlink`, keep the cap under 65,000 or accept
  that such ids fail at that shard's limit. Not tested on such a filesystem here (tmpfs and
  the VM's root have no such limit).

## A full disk

A request to an object without storage gets 507 `disk-full` when `statfs` says the
filesystem has less free than the object's quota (`disk_mb`, and `max_files` inodes where
the filesystem counts them); so does the first create. `statfs` joined the worker's
seccomp list.

`a_full_disk_makes_no_new_objects` (root: mounts a 4 MiB tmpfs as the data directory, the
audit's scenario scaled down): `POST /oN/incr` on new counters (`disk_mb` 1) until the
first refusal. **257 counters fit; the 258th got 507 `disk-full`** ("the disk has less free
than a new object may use (1 MiB, 100 files)"); 100 more new objects, each written and read,
were refused and made nothing; `o0` still answers. With the disk check taken out the same
test fails as the audit did: `500 commit-failed` once the disk is full.

What this does not do: reserve room for existing objects' writes, or refuse the node's own
writes near full. That is the disk budget (`beta/limits`).

## Delete

- **By the object:** remove its files. The empty volume goes when the instance ends; no
  new WIT. With capture an emptied object keeps its history (its next version must follow
  the store's head), so its directory stays until an operator deletes it.
- **By an operator:** `nanovm admin delete <app> <object>`, `nanovm admin delete-app <app>`.
  The supervisor drains and stops the app's worker (as a version switch does), holds the
  shipper off (`ship::run` ships each object under a lock the delete takes), removes the
  store's keys (`<app>/<object>/` or `<app>/`: head and versions; `Store::remove`), then
  moves the node's copy into `<data_dir>/deleted` (one rename, synced) and removes it. The
  rest of a delete cut short goes when the supervisor next starts. Refused with leases
  (B-206).
- `an_operator_deletes_an_object_and_an_app` (jailed worker, directory store): after
  `a` twice and `b` once are shipped, `delete <app> a` removes `a` here and in the store,
  leaves `b`; `a` reads 0, and a write ships again (a new history from 1). `delete-app`
  leaves none of the app's three directories, nothing in the store, nothing in `deleted`,
  and the app serves again from nothing. Bad names are refused.
- **No automatic deletion of data:** nothing removes a volume that holds anything.

## The control path

`<data_dir>/admin/socket`, in a 0700 directory that must be the supervisor's own; bound and
reached as `/proc/self/fd/<fd>/socket` through that directory, so a long `data_dir` fits
(recovery's B-508). A command is a line of JSON (an array of strings), the answer one line.

- `admin_socket_is_the_supervisors_own`: `apps` through the CLI; uid 65534 gets `EACCES`;
  `/_nanovm/admin`, `/command`, `/delete` are 404 without an app and 400 with one.
- `jail_blocks_escapes`: a jailed worker can't connect to a listening Unix socket
  (`admin_socket=denied`; it can't make a socket at all).
- `the_admin_socket_takes_a_long_data_dir`: a 183-byte socket path; `apps` works.

## Mutation checks

Each mechanism broken on its own, the named test run, then `git checkout -- <file>` and an
empty `git diff`:

| Mutation | Caught by |
|---|---|
| volume made up front again (`Journal::open`) | `objects_that_change_nothing_leave_nothing` ("while live") |
| no rollback of a new, never-committed object when its instance ends | `failed_and_emptied_objects_leave_nothing` (failed objects stay) |
| no `settle` when an instance ends | `failed_and_emptied_objects_leave_nothing` |
| every failed instance rolled back as it ends, not just a new object's | `nodes::a_node_that_lost_an_objects_lease_cannot_commit_it` (its uncommitted write is gone before the next start) |
| `settle` ignores captured history | `only_empty_objects_go` |
| cap taken without the atomic check | `room_for_objects` (16 of 16 admitted) |
| no disk check | `a_full_disk_makes_no_new_objects` (500 `commit-failed`), `room_for_objects` |
| delete leaves the store | `an_operator_deletes_an_object_and_an_app` |
| no shards | `objects_are_sharded` |
| admin directory and socket world-writable | `admin_socket_is_the_supervisors_own` (mode, and uid 65534 connected) |
| jail allows `socket`/`connect` | `jail_blocks_escapes` |
| socket bound by its full path | `the_admin_socket_takes_a_long_data_dir` |

## CPU per request

Process CPU (`getrusage`, user + system) per request, in-process node, `counter`, 64
clients, release builds of `cf/round3` (7b3f9c6, "old") and this branch ("new"), alternating
old/new for 4 rounds; median [range] in µs. The program (`objects_cpu.rs`, not committed:
copied into each checkout's `crates/nanovm/examples/`) runs, on one node: `GET /` on 2,000
new objects (cold start, no change), `POST /incr` on 2,000 new objects (the first write
makes the object), then 100,000 warm `GET /` over 1,000 objects and 10,000 warm `POST /incr`
over 1,000. Load average 7 to 11 from other agents throughout, so only large differences
would show.

| Path | Old | New | Change |
|---|---|---|---|
| read of a new object (cold start) | 551 [456–648] | 552 [474–559] | +0.2% |
| first write of a new object | 1,602 [1,494–1,778] | 1,574 [1,321–1,711] | −1.8% |
| warm read | 98 [82–104] | 94 [82–105] | −4.3% |
| warm write | 823 [710–895] | 798 [718–808] | −3.0% |

No difference beyond the noise. The warm paths don't run any of the new code (an existing
object's volume exists, so `place` returns at once). A cold start of a new object trades a
`mkdir` for a `stat`, a pool directory and a `statfs`; its first write adds a `rename` and
two directory syncs (the shard and the app directory, and the object's directory at
commit); a cold start of an existing object a `stat` for the `mkdir` that failed.
