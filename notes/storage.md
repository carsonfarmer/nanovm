# Storage: cheaper durable writes, and committed versions off the node

Branch `cf/storage` (from `cf/main-next`). ISSUES N-160..166, W-80.

## What a durable write cost, and why

A counter `POST /incr` creates `count.tmp`, writes it and renames it over
`count`. Before this work it made 6 fsync + 3 fdatasync calls (strace -c), and
on a private nojournal ext4 loop device 23 writes, 2 discards and 9 flushes
(journaled ext4: 16.9 writes, 12.7 flushes):

- the request's first undo record created the log and synced three directories;
- every record was synced on its own (the created file; the replaced file moved
  into the journal; the rename);
- commit synced each changed path, deleted the log and synced its directory
  (deleting freed the log's block: one discard per request on a `discard` mount).

Two more things capped throughput: every sync ran on a tokio worker thread,
so a 4-thread worker had at most 4 syncs in flight, though this disk does 543
fdatasync/s from one thread and 11k/s from four (34k/s from 64); and a guest's
own `sync` reached the disk (SQLite syncs ~1.7 times per statement).

## What changed (journal.rs, quota.rs)

- **The log is kept.** It is made once per object (under another name, then
  renamed, so it is always whole), never deleted. Its first 4 KiB block holds a
  format tag (`nanovmj1`) and a random 8-byte key; records start in the next
  block. Each record is signed: the first 8 bytes of SHA-256 over the key and
  the record. Recovery reads records from the second block until one isn't
  signed with the key in the header.
- **A commit replaces the key**, writing and syncing that one existing block.
  No record is signed with the new key, so every old one is void. What lies past
  the current request's records is left from earlier ones (some of it guest
  bytes from old `Bytes` records), but it can't be signed without the key, which
  is random and never leaves the log. After a commit the log is cut back to
  68 KiB if a big request grew it.
- **Records made back to back share one sync**: a file's size and its
  overwritten bytes; a rename and the file it replaces.
- **The commit runs on a blocking thread**, awaited, and syncs the changed
  paths up to 16 at a time. A request that changed nothing commits on its own
  task (handing it over cost a read ~130 µs of CPU). `block_in_place` was tried
  first and hung a post-return test: don't use it inside wasmtime's event loop.
- **A guest's `sync`/`sync-data` do nothing**: durability is the commit's.

`/incr` now: 5 flushes (the created file; the rename with the replaced file;
`count`; the directory; the key), 15 writes, 1 discard (the replaced file, removed
after commit). Journaled ext4: 9.5 writes, 5.5 flushes.

The crash protocol changed, so: the existing crash-cut tests pass (unit sync
failures at every point of record/commit/recover, recovery interrupted at every
undo, Amp's 14 commit + 28 rollback capture cuts including subprocess exits,
`sqlite_composed_recovery_cuts` 114 VFS cuts, `sqlite_journal_pressure_matrix`
in DELETE/TRUNCATE/PERSIST/MEMORY/WAL), and new tests cover what the new
protocol adds, each mutation-checked: records past the request's (an old key, a
guessed key) are not undone; a log in another format is refused, not taken as
empty; a log cut short while being made is ignored; the log shrinks after a big
request; sync counts per operation; a commit doesn't block its runtime (one
worker thread) nor wait for a blocking thread when there's nothing to commit;
parallel commit syncs are faster and fail if any path fails.

Upgrade note: a log left uncommitted by a binary before this format is refused
(InvalidData); drain cleanly before upgrading.

## Numbers

Device I/O per request, bench `--writes --quick`, private loop devices (only the
bench uses them, so the counters are exact):

| per request | nojournal ext4 (discard) | journaled ext4 |
|---|---|---|
| `/incr` before | 23.0 writes, 2 discards, 9 flushes | 16.9 writes, 12.7 flushes |
| `/incr` after | 15.0 writes, 1 discard, 5 flushes | 9.5 writes, 5.5 flushes |
| `/set` (SQL+file+KV) before | 27 writes, 1 discard, 11 flushes | |
| `/set` after | 18.7 writes, 7 flushes | |

Real disk (this VM's nojournal ext4, load average 10-25 from other agents), 3
alternating rounds, dev profile, medians (ranges): `POST /incr` 256-concurrent
over 100 objects 388/s (350-625) -> 1,057/s (1,023-1,229), p99 2.6 s -> 0.98 s;
serial 40/s (34-87) -> 75/s (72-80); process CPU per write 1,069 µs -> 848 µs
concurrent, 1,718 -> 1,346 µs serial. Wall-clock numbers here swing several-fold
with other agents' I/O; trust the device counts.

Reproduce: `cargo build --example bench`; for exact counts make a loop device
(`truncate -s 2G x.img; mkfs.ext4 -O ^has_journal x.img; losetup --direct-io=on
-f --show x.img; mount -o discard ...`) and run
`NANOVM_BENCH_DATA=<mount>/bd NANOVM_BENCH_DEV=loopN bench --writes 1 --quick`
(add `NANOVM_BENCH_CAPTURE=1` for capture). The `/set` row needs
`components/sqlite/build.sh`.

## Committed versions off the node (committed.rs, ship.rs)

On top of Amp's W-9 local capture (b7bf806):

- **Versions are changes** (format 2): the whole volume every `snapshot_every`
  (16) versions, otherwise what the commit changed, taken from what the journal
  already tracks: final listings of directories whose entries changed; new
  files and anything a rename moved, whole (directory trees too); files changed
  in place as their changed byte ranges. `committed::restore` rebuilds a volume
  from the last snapshot and the changes after it, checking parent digests; a
  version naming a path outside the root is refused. A 1-byte change to a 1 MiB
  file is a 7 KiB version, not 1 MiB.
- **Staging costs 2 syncs, not 8.** A version is staged as
  `~committed/<object>/<seq>.<decision>.staged`, the decision being a hash of
  the journal key its commit replaces; it is published by an unsynced rename in
  the same directory after the key's sync. Recovery publishes a staged version
  whose key is gone (after syncing the log, since a key it sees may not be
  durable) and discards, durably before any undo, one staged under the current
  key. With capture, `/incr`: 17 flushes and 39.8 writes -> 7 and 20.9; `/set`:
  19 flushes -> 9.
- **Shipping.** With `capture.store` set (a directory, or an object store URL as
  for `leases`: round3's `object_store`, S3 and compatibles), the supervisor
  (jailed workers have no network), or an in-process node, ships each object's
  versions in order to `<app>/<object>/<seq>.tar` with put-once semantics (hard
  link; `PutMode::Create`), then removes them locally but the last (the next
  one's parent). A key already holding other bytes (another writer) stops
  shipping. On cf/storage, before round3, this had its own SigV4 client and a
  fence object; on cf/storage-r3 both gave way to `object_store` and the lease.
- **Under the lease (W-90, cf/storage-r3).** Versions (format 3) carry the lease
  epoch they were committed under; the lease is checked right before the
  journal's new key, so a lapsed lease refuses the commit and recovery discards
  what capture staged. The shipper reads the object's lease record and ships a
  version only if it names this node or still has the version's epoch:
  `a_node_that_lost_the_lease_cannot_ship` shows a node's late version refused
  while the new owner's ship, and a third root restoring the new owner's
  history. Remaining: the window between reading the record and the put. The capture limits now bound the unshipped outbox: with the store
  down, writes are refused once it is full; nothing is dropped.
- **Acknowledgement** stays local by default. `remote_ack = true` holds each
  commit until its version is stored (2 s at most; then the request fails, but
  the commit stands locally: ambiguous, not a rollback).
- **Restore.** `nanovm::ship::restore` rebuilds an object from the store on a
  fresh data root and keeps its last version so history goes on;
  `shipped_versions_restore_an_object_on_a_fresh_root` serves it and ships the
  next version from there, twice.

Tests (mutation-checked): 120 random steps of creates, overwrites, growth,
truncation, removes, file and directory renames (over files and empty
directories) and type changes, every version restored and compared with the
live volume; small-change size; a directory renamed over an emptied one;
unsafe paths; put-once for both stores against an in-process S3 fake that checks
signatures; the shipper's order, pruning, conflict and fence; outage: no
acknowledgement under `remote_ack`, a full outbox refusing writes.

## N-600 (SQLite provider syncs)

The provider's syncs reached the host's fsync. Making a guest's sync a no-op
(N-163) fixes that for every guest: 120 fewer fsync calls over 70 SQL writes.
Changing the provider to `journal_mode=MEMORY` would also save its per-
transaction journal file (one record sync); not done here.

## W-600 (rollback with a chosen status)

Doesn't fall out of the journal work: a guest still can't roll back and choose
its status. The journal could expose "roll back now" as a host call that
replays the log and restarts nothing, but that is a guest API decision.

## Open (ISSUES)

N-162 (2 record syncs per KV key: bulk KV tests time out under heavy load, as on
base), N-165 (the shipper polls), N-166 (restore reads every version; needs a
snapshot pointer and remote retention), W-80 (a `nanovm restore` command,
commit-time fencing, the RPO decision).
