# Storage: cheaper durable writes, and committed versions off the node

Branch `cf/storage` (from `cf/main-next`). ISSUES N-160..166, W-80.

## What a durable write cost, and why

A counter `POST /incr` creates `count.tmp`, writes it and renames it over
`count`. Before this work it made 6 fsync + 3 fdatasync calls (strace -c), and
on a private nojournal ext4 loop device 23 writes, 2 discards and 9 flushes
(journaled ext4: 16.9 writes, 12.7 flushes; the device counts are the authors',
see Numbers):

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

`/incr` now: 5 sync calls (the created file; the rename with the replaced file;
`count`; the directory; the key). On the authors' loop device, 15 writes, 5 flushes
and 1 discard (the replaced file, removed after commit); journaled ext4: 9.5 writes,
5.5 flushes.

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

### Upgrading from main's capture (format 1, S2)

Main staged each version as `~journal/<object>/capture` (the whole volume), deleted
the undo log (the decision), then hard-linked the capture to
`~committed/<object>/<seq>.tar` and removed it. The first open under this format:

- **Main's log still there** (its commit undecided): refused, InvalidData, with the
  log and the staged capture kept. Drain main (or let it recover) before upgrading.
- **A staged capture without main's log**: main decided that commit. It is published
  as main's `finish` would have (sync the journal directory, check the identity,
  sequence and parent, hard link, sync, remove) before anything in the journal
  directory is cleared. Before, recovery cleared it as a leftover: the volume kept
  a commit no version held, and the next version's parent skipped it.
- **Main's published history** is migrated by reading it, not rewriting it: a
  format-1 version is a whole volume with no epoch, so it reads as a snapshot at
  epoch 0 (a single node's epoch; main had no leases). The next version is format 3,
  a change to main's last (its parent that file's digest) unless its sequence falls
  on a snapshot; `restore` starts from the last snapshot of either format. Main's
  files and digests stay valid, and a shipper ships them with the rest; with leases,
  epoch 0 matches no lease record, so they ship only while the record names this
  node. Before, the first format-3 change made the history unrestorable (`restore`
  refused main's versions: "missing field `epoch`").

`review_upgrade_preserves_decided_legacy_capture` builds main's layout byte for byte
(its `prepare`'s tar, its `finish`'s link) and checks both; a second test checks the
refusal keeps main's log and capture.

## Numbers

Sync calls per write (fsync and fdatasync, strace -c): `/incr` 9 -> 5, with
capture 17 -> 7. An independent audit reproduced these exactly. They are
syscalls, not device flushes: the audit could not reproduce the flush counts
below on virtual storage, where a sync need not reach the device as one flush.

Device I/O per request as the authors measured it, bench `--writes --quick`,
private loop devices (only the bench used them); not reproduced by the audit:

| per request | nojournal ext4 (discard) | journaled ext4 |
|---|---|---|
| `/incr` before | 23.0 writes, 2 discards, 9 flushes | 16.9 writes, 12.7 flushes |
| `/incr` after | 15.0 writes, 1 discard, 5 flushes | 9.5 writes, 5.5 flushes |
| `/set` (SQL+file+KV) before | 27 writes, 1 discard, 11 flushes | |
| `/set` after | 18.7 writes, 7 flushes | |

Throughput and CPU, from the audit: an idle 4-vCPU machine, 3 alternating
pairs, medians [ranges]. Concurrent writes (`POST /incr`) 784/s [763-797] ->
3,074/s [3,031-3,094], p99 1,407 ms -> 302 ms, process CPU per write
932 µs -> 749.5 µs (-19.6%); serial 323/s -> 530/s, CPU per write 810 µs -> 820 µs
(+1.2%, ranges overlap). These replace the authors' figures where they conflict:
round3's notes had serial writes flat (385 -> 394/s) at about 12% more CPU, and
this note had serial CPU falling (1,718 -> 1,346 µs); neither reproduced. The
authors' run on a loaded VM (load average 10-25 from other agents) had concurrent
388/s -> 1,057/s, p99 2.6 s -> 0.98 s and serial 40/s -> 75/s; wall-clock numbers
there swung several-fold with other agents' I/O. Trust the sync call counts and
the audit's numbers.

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
  key. With capture, `/incr`: 17 sync calls -> 7 (reproduced by the audit); on the
  authors' loop device 39.8 writes -> 20.9, and `/set` 19 flushes -> 9 (not
  reproduced).
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

  key. With capture, `/incr`: 17 flushes and 39.8 writes -> 7 and 20.9; `/set`:
  19 flushes -> 9.
- **Shipping.** With `capture.store` set (a directory or `file:///path`, or an
  object store URL as for `leases`: round3's `object_store`, S3 and compatibles),
  the supervisor (jailed workers have no network), or an in-process node, ships
  each object's versions in order, then removes them locally but the last (the
  next one's parent). A version is put once at
  `<app>/<object>/<seq>-<sha256>.tar`: the key names the bytes, so writers never
  clash (a hard link from a fresh `O_EXCL` temporary file, synced, then the
  directory synced; `PutMode::Create`). On cf/storage, before round3, this had
  its own SigV4 client and a fence object; on cf/storage-r3 both gave way to
  `object_store` and the lease.
- **The head (W-90, fix/lease-r3fix).** A version counts only once the object's
  head, `<app>/<object>/head` = `{sequence, digest, node, lease}`, names it. The
  shipper moves the head one version at a time, to the next sequence whose parent
  digest is the head's, by a write conditional on the head it read (`If-Match`
  the ETag; on a directory store a rename under a lock file, compared by
  digest), and only then marks the version shipped locally. With leases it does
  so only under this node's live lease on the object (a grant its worker holds,
  in the node's current session) and with the head claimed by this node at that
  lease's epoch: before a worker gets a lease, its supervisor claims the head
  (`ship::claim`), so no node with an earlier lease can move it, however late its
  upload lands. The claim also makes the node's copy of the object the head's
  (kept if it holds the head's version and no other node has claimed the object
  since this one did, or it is exactly the head's; otherwise dropped and
  restored), so a takeover never serves state older than the head and never ships
  history the head has moved past. Versions (format 3) carry the lease epoch they
  were committed under; the lease is still checked right before the journal's new
  key, so a lapsed lease refuses the commit and recovery discards what capture
  staged. The capture limits bound the unshipped outbox: with the store down,
  writes are refused once it is full; nothing is dropped. See
  `notes/multinode.md` for the argument.
- **Acknowledgement** stays local by default on one node. `remote_ack = true`
  holds each commit until the head names its version (2 s at most; then the
  request fails, but the commit stands locally: ambiguous, not a rollback). With
  `leases` a config file defaults it to true (owner's decision): a takeover
  restores from the store, so a write answered only locally would be lost.
- **Restore.** `nanovm::ship::restore` rebuilds an object on a fresh data root
  from the head back, parent by parent, to the last snapshot (only those versions
  are fetched), syncs the volume, then keeps the head's version so history goes
  on; `shipped_versions_restore_an_object_on_a_fresh_root` serves it and ships the
  next version from there, twice. A claim restores the same way.

Tests (mutation-checked): 120 random steps of creates, overwrites, growth,
truncation, removes, file and directory renames (over files and empty
directories) and type changes, every version restored and compared with the
live volume; small-change size; a directory renamed over an emptied one;
unsafe paths; put-once for both stores against an in-process S3 fake that checks
signatures; the shipper's order, pruning and head; outage: no
acknowledgement under `remote_ack`, a full outbox refusing writes. The head's
fence, authority and takeover: `review_takeover_during_upload_fences_the_old_writer`,
`review_shipping_requires_a_live_matching_epoch_not_just_a_node_name`,
`a_claim_keeps_only_history_no_other_node_has_moved_past`,
`review_concurrent_directory_put_keeps_acknowledged_bytes` and
`nodes::review_failover_with_separate_disks_preserves_remote_acknowledged_state`.

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
base), N-165 (the shipper polls), N-166 (restore now fetches only from the last
snapshot before the head; remote retention remains), W-80 (a `nanovm restore`
command).
