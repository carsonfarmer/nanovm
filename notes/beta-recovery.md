# Beta: recovery (`beta/recovery`)

The audit's gap: recovery was offline and whole-node only. `scripts/backup.py`
(restic) needed the node stopped; a restore replaced the whole root, so every
tenant lost what it wrote since the snapshot; round 3's store had no restore
command; nothing restored one app or object; and with a store on one node, a
store outage became a write outage after 16 versions. Issues B-500 to B-506.

## Decision: the backup is the store

| | restic (`scripts/backup.py`) | shipping to `capture.store` |
|---|---|---|
| Node during backup | stopped, every writer | serving |
| Granularity | whole node | each committed version of each object |
| Loss on node loss | everything since the last snapshot | what wasn't shipped yet: tens of ms with the store up (`backlog`) |
| Restore | whole root, offline, every tenant back in time | one object or one app, live, to its head or any version kept |
| Checks | restic's | each version's SHA-256 is its key; the chain of parents |
| Extra moving parts | Python, restic, an operator schedule | none: it runs whenever capture has a store |

Restic copied nothing durable that the store doesn't hold: it skipped scratch,
caches and the registry; journals are transient (a restore discards the old
volume's). So, under AGENTS.md rule 1 (no old way kept for its own sake),
`scripts/backup.py`, its Python tests, the CI step and the restic drill are gone;
the drill's SQLite crash scenario now restores a lost node from the store
(`sqlite_restores_a_lost_node_from_the_store`).

What the store doesn't cover: apps themselves (signed OCI artifacts; keep the
registry and DNS records), and an object's state from before capture was turned on
(turn it on before the first write; operators start fresh anyway, rule 1).

## Restore into a running node

`nanovm admin … restore <app> [<object> [<version>]] [--discard-unshipped]`, on
the objects agent's control path (`origin/beta/objects` 1b11455, merged): a
root-only Unix socket on the supervisor. `versions [<app> [<object>]]` browses
the store; `backlog` says what waits to ship.

Per object (`Supervisor::restore`, `ship::restore`):

1. **Pause** (`ship::pause`): a file `objects/<app>/~paused/<object>`. A worker
   checks it before it starts the object's instance (`object::run`; one `stat`
   per cold start, not on warm requests) and answers `503 object-restoring`. The
   supervisor then asks the app's worker, if it runs, to stop the instance
   (`/_nanovm/evict/<id>` on the supervisor's own link: `Node::evict` drops the
   table entry and waits until the actor and its store are gone). Being on disk,
   a pause holds across a worker restart or version switch.
2. With leases, take the object's lease and claim its head (`ship::claim`), as a
   worker's start does; another node holding it refuses the restore.
3. **Ship the backlog** (`ship_object`, three tries: the background shipper may
   move the head under one). Refused by the store → the restore stops, unless
   `--discard-unshipped`.
4. **Stage** under `<data_dir>/restore/<app>/<object>`: fetch back from the head
   to the version asked for and on to the last snapshot, each checked against
   the digest that names it and against its own identity (app, object, sequence);
   rebuild the volume (`committed::restore`). If the version is older than the
   head, make a snapshot of it the version after the head (`committed::prepare`
   with no changes), keeping the head's version as its parent. Give it all to the
   app's uid, `syncfs`.
5. **Decide**: write `ready`, sync. **Install** by descriptor: remove the
   object's journal (its undo was for the old volume), swap the volume, mark the
   first staged version shipped, swap the versions directory, `syncfs`, remove
   the staging. Each step can be taken again.
6. Ship the version made, if one was (three tries), before letting the lease
   go: with leases only a held lease lets it ship, and a failover before it
   counts would restore the old head. Release the lease, resume.

`ship::recover`, before the supervisor starts any worker, installs each staged
restore that has `ready` and removes the others, then removes every pause.

**Going backwards.** A restore of version `N` below the head `H` makes version
`H+1` with `N`'s contents. The store's head only ever moves forward (every
writer, lease and remote ack assumes it), versions `N+1..H` stay restorable (undo
= restore `H`), and the command's answer says so. Restoring the head itself makes
no version.

**Retention.** The store keeps everything today, so any version is restorable.
With retention (N-166: drop versions before the second-last snapshot), a version
stays restorable while the store keeps it, the snapshot before it, and every
version after it (the restore follows parents back from the head; B-506).

## Store outages on one node (B-501)

`max_versions` (16) is gone; `max_mb` (bytes, per object, default 256 MiB) bounds
what waits. With versions as changes and a snapshot every 16, an outage's backlog
still grew with the volume (one copy per 16 writes), so a scheduled snapshot is
now skipped while more than `snapshot_every` versions wait. Multi-node
`remote_ack` behaviour is unchanged (a write fails after 2 s without the store).

Visibility: `ship::backlog` (versions after each object's shipped marker), counted
by the supervisor every 10 s into its OTLP snapshots
(`nanovm.capture.waiting.versions|bytes|objects`), and a log line when versions
start to wait, every minute while they do, and when none does; `admin backlog`
counts now. Readiness (`/_nanovm/ready`) is not affected on purpose: failing it
would take a node that still commits writes out of service.

`writes_go_on_through_a_store_outage_and_ship_in_order_after`: a 200 KiB file and
a counter; the store replaced by a file (puts fail `ENOTDIR`); 59 writes to one
object and 1 to another all answer 200; `backlog` = 60 versions, 2 objects,
**451,072 bytes**; the store back: within the 20 ms rounds every version ships,
each the parent of the next (walked from the head to version 1), and a fresh
root restores the counter at 60. Snapshots in the store: versions 1 and 17 only
(17 had 15 waiting); with the skip removed (mutation M1) they are 1, 17, 33 and
49: two more copies of the volume in the backlog.

## Security findings on the way

- **B-502 (from source):** with leases a claim restored as root into
  `objects/<app>/~restore/`, which the app's (possibly compromised) worker owns:
  `create_dir_all` and `fs::write` follow a symlink it plants there, so root
  wrote the app's bytes where the worker chose. Restores now stage outside every
  jail and install by descriptor (`openat` + `O_NOFOLLOW` per name, `renameat`,
  `remove_at`). `mark` and `discard` have the same shape and are being rewritten
  on `fix/r3-lease`; recorded there for them.
- **B-504 (from source):** the shipper didn't check that a version names its
  app and object, so a worker could get another object's (or app's) version
  shipped and restored as its own. Now checked when shipping and restoring.
- **B-503 (reproduced):** the end-to-end test's other app failed a write with
  `gap in committed versions`: the worker listed its versions while the shipper
  pruned them. Fixed in `committed::head`.

## Tests

| Test | What it shows |
|---|---|
| `restores_an_object_and_an_app_into_a_running_node` | Supervisor, two apps (jailed workers, root), through `nanovm admin`: app B writes throughout and never misses (each answer the next count); A's object to version 3 of 5 (made 6; answer says 4–5 stay), served at 3; `versions`; a damaged volume back to the head; a paused object answers `503 object-restoring` while its app's other object serves; a second restore of a paused object refused; the whole app; a local version the store refuses stops a restore and `--discard-unshipped` drops it; bad app and object names, a version past the head, a damaged version in the store refused, nothing changed; `/_nanovm/evict` unreachable from a client (404) or through an app (400); everything ships, `backlog` empties. |
| `a_restore_cut_short_is_finished_or_undone` | A crash at each of 9 points, then `recover`: before `ready`, the object, its journal and versions are as they were; after, restored as the version after the head, which ships; nothing staged, nothing paused. |
| `writes_go_on_through_a_store_outage_and_ship_in_order_after` | Above. |
| `an_unreachable_store_withholds_acknowledgement_and_bounds_the_outbox` | `remote_ack` still fails the request (commit stands); without it, 300 KiB writes go on until `max_mb` (1 MiB), then `commit-failed`; every version kept; `backlog` = 4 versions. |
| `a_commit_goes_on_past_versions_pruned_out_of_order` | B-503. |
| `a_version_naming_another_object_is_not_shipped` | B-504. |
| `nodes::an_object_is_restored_on_the_node_holding_it` | With leases: the node not holding the object refuses; the holder restores, the proxy serves it, history goes on. |
| `sqlite_restores_a_lost_node_from_the_store` (ignored: SQLite fixture) | A SQLite app's worker killed mid-request, the node's data deleted; a fresh node restores the app through `nanovm admin`: committed rows, file and key; then a write that survives a restart. |

Mutation checks (each applied alone or with others caught by other tests, then
`git checkout -- crates/nanovm/src`, `git diff` empty):

| | Mutation | Caught by |
|---|---|---|
| M1 | snapshot every 16 regardless of the backlog | outage test (`[49, 33, 17, 1]`) |
| M2 | refuse at 16 waiting versions again | outage test (write 17); also the end-to-end test's other app, with the store up: refused at its 82nd write, the shipper's 20 ms rounds having fallen 16 versions behind a steady writer |
| M3 | ignore the pause when an instance starts | end-to-end (200, not 503) |
| M4 | keep the old journal when installing | cut test |
| M5 | `recover` installs undecided restores | cut test |
| M6 | require contiguous versions again (B-503) | pruned-out-of-order test |
| M7 | ship a version naming another app or object | identity test |
| M8 | evict does nothing | end-to-end |
| M9 | a restore ignores what it couldn't ship | end-to-end |

## Runtime lines

CI's method: `cf/round3` (7b3f9c6) 7,418; with the merged control path 7,496;
this branch 7,806: **+310** (ship.rs restore, install, recover, pause, backlog,
`Store::list`: +173; supervisor restore, pause, commands, backlog count, shipping
the made version: +124; worker pause check and evict: +14; `committed`,
`config`, `journal`: −2).

## Not done

- Online restore with leases is tested on two in-process supervisors with an
  in-memory lease store and a directory capture store, not against S3.
- `mark` and `discard` still write and remove through paths as root (B-502):
  left to `fix/r3-lease`, which is rewriting them.
- A fresh node doesn't restore objects by itself (B-505): the runbook restores
  them with ingress off.
- A restore to an old version fetches every version after it whole (B-506).
- No retention (N-166), so no test of a version the store no longer keeps.
