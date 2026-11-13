# Runbook: backup and restore

For an operator of a single node (and, where it says so, several). What each
command does and why is in [README.md](../README.md#backup-and-restore); the
evidence in [beta-recovery.md](beta-recovery.md).

All commands run on the node's host as the node's user (root), against the node's
config: `nanovm admin --config /etc/nanovm/nanovm.toml <command>`, below
`admin <command>`. They need the supervisor running.

## Before the first write: turn on the backup

1. Choose a store off the node: an S3 bucket (`s3://bucket/prefix`, credentials
   and region from `AWS_*` in the supervisor's environment), or a directory on
   another disk. Turn on bucket versioning or object lock if you want the store to
   survive its own credentials being misused; nanovm never deletes from it.
2. In the node's config:

   ```toml
   [capture]
   store = "s3://bucket/nanovm"
   ```

3. Start the node with an empty `data_dir`. Every object is in the store from its
   first commit. (Capture can't be turned off on a root with history, and data
   written before it was on is not in the store.)

Size the disk: per object, `disk_mb`, up to 2 × `disk_mb` more while a request
runs, and up to `capture.max_mb` (256 MiB) while the store is unreachable.

## Every day: is the backup current?

- `admin backlog` → `{"versions": 0, "bytes": 0, "objects": 0}` when everything
  committed is in the store. Alert when `nanovm.capture.waiting.versions` (in the
  supervisor's OTLP snapshots) stays above 0 for more than a minute.
- The log says `nanovm: N versions (B bytes) of K objects wait to be shipped` when
  versions start waiting and every minute while they do, and `nanovm: every
  version is shipped` when they stop; `nanovm: shipping <app>/<object>: <why>`
  says why each object's versions wait.

## The store is down

Nothing to do for writes: the node goes on answering them, durable on its own
disk. Watch `admin backlog`: an object whose waiting versions reach `max_mb`
refuses writes (`500 commit-failed`) until the store is back. Raise `max_mb` (a
restart) if the outage will outlast it and the disk has room. When the store is
back, everything ships, in order, by itself; `backlog` goes back to 0.

With `remote_ack = true` (several nodes), writes fail after 2 s while the store
is down: that is the multi-node contract, unchanged.

## An object's data is bad (an app bug, a bad deploy, a mistake)

1. `admin versions <app> <object>`: the head and each version with when it was
   stored (seconds since 1970). Pick the last good version, `N`.
2. `admin restore <app> <object> N`. The object answers 503 for the few seconds
   it takes; its app's other objects and other apps keep serving. The answer says
   which version was made: restoring an older version makes a new one after the
   head, with N's contents, so nothing is lost: to undo, restore the old head.
3. Check the object through the app.

To restore every object of an app to its head (its data on this node is damaged,
or was deleted): `admin restore <app>`.

## The restore refused

- `… has versions the store doesn't, which a restore would drop`: the object
  committed here what the store hasn't got and won't take (the store is down, or
  refuses what is here). If the store is down, wait for it. If what is here is
  wrong (the object was written on a fresh node before its restore, say), add
  `--discard-unshipped` to drop it.
- `… isn't what its key names` or `… is not in the store`: the store's copy of a
  version is damaged or missing. Nothing changed on the node. Restore another
  version, or repair the store (from its own versioning) and retry.
- `… has versions 1 to H`: there is no such version.
- `… is being restored already`: another restore of it is running.
- With leases, `taking <app>/<object>'s lease: Held("<node>")`: run it on that
  node.

## The node crashed during a restore

Start it again. Before any worker starts, the supervisor finishes each restore
that was decided and drops the others; the object is then either restored or as
it was, and serving. Run the restore again if it isn't what you wanted.

## The node is lost

1. Fence the old node: make sure it can't come back and write (power it off,
   revoke its store credentials).
2. Start a new node with the same config (same app trust policies and egress) and
   an empty `data_dir`, with ingress off (don't point DNS or the proxy at it yet).
3. `admin versions` lists the apps in the store; for each, `admin restore <app>`.
4. Check a few objects through the proxy pointed at the new node only, then open
   ingress.

What was lost is what the old node had committed and not shipped: `backlog` on
the old node, if it could still be read, says how much.

Apps themselves (their signed OCI artifacts) are not in the store: keep the
registry and the DNS records; nodes pull apps again on first request.
