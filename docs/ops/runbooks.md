# Runbooks

For one node with the [systemd units](../../deploy/systemd/README.md):
`nanovm.service` (node, `data_dir = /var/lib/nanovm`, listening on
127.0.0.1:8080), `nanovm-proxy.service` (8443) behind a TLS terminator; the
[single-node guide](../../deploy/single-node/README.md) sets this up. In a
container, read `docker stop`/`docker start` for `systemctl`. Each step was
checked against the code; the drills in [drills/](drills) reproduce the
behaviour quoted, and were run again on round 3
([notes/beta-deployable.md](../../notes/beta-deployable.md)).

Three facts every runbook leans on:

- **Stopping is not graceful.** SIGTERM ends the supervisor; workers die with it
  and in-flight requests roll back, their clients unsure whether they committed.
  Close ingress at the TLS terminator first.
- **Never edit or delete anything in an object's directory by hand.** Each
  object is `objects/<app>/<shard>/<object>/` (README, "Data lifecycle"):
  `data` is its volume, `journal` a request's undo record (the object recovers
  it on its next start; deleting it can leave half a request's writes in
  place), and with capture (`[capture]`, off by default) `committed` is its
  version history and `shipped` what has reached the `store` (nanovm removes
  shipped versions itself, all but the last). Delete an object or an app with
  `nanovm admin delete` / `delete-app` on the running node, which also
  removes what it shipped. Don't remove `[capture]` from a node that has
  history (its objects then refuse to start: `committed history requires
  capture to remain enabled`).
- **Uids are data.** An app's uid is the owner of its `objects/<app>` directory.
  Keep `jail_uid_base`, run as root, and preserve numeric owners.

## Upgrade nanovm

There is one version of nanovm, v1 ([AGENTS.md](../../AGENTS.md), rule 1): a
new build may change its on-disk formats in place, and data an earlier build
wrote is not supported. An upgrade therefore starts the node on a fresh data
directory, and keeps the old one, with the old binary, for a rollback. Tell app
owners first: their objects start empty.

1. Read the release notes for config keys (an unknown key stops the node from
   starting; `tests/configs.rs` keeps the shipped configs current) and host
   requirements. Run `deploy/single-node/check-host.py` against the new config.
2. Close ingress at the terminator. `systemctl stop nanovm-proxy nanovm`.
3. Set the old data directory aside, and install the new binary **only now**,
   keeping the old one:
   ```sh
   mv /var/lib/nanovm /var/lib/nanovm.prev
   cp -p /usr/local/bin/nanovm /usr/local/bin/nanovm.prev
   install -m755 nanovm /usr/local/bin/nanovm.new && mv /usr/local/bin/nanovm.new /usr/local/bin/nanovm
   ```
   A running supervisor starts its workers from its own image (N-552), so a
   binary replaced under it takes effect only when the node restarts; `cp` over
   the running file fails with "Text file busy", `install`/`mv` succeed.
4. `systemctl start nanovm nanovm-proxy`, then
   `curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8080/_nanovm/ready` (204).
   Startup errors are in `journalctl -u nanovm`.
5. Through the proxy, read and write one object of a canary app; check the
   `supervisor.request` error rate. Open ingress.

## Roll back nanovm

1. Close ingress; `systemctl stop nanovm-proxy nanovm`.
2. Put back the old binary and the data directory it wrote. Writes made since
   the upgrade stay in the new build's directory; the old build doesn't read it.
   ```sh
   mv /var/lib/nanovm /var/lib/nanovm.rolled-back && mv /var/lib/nanovm.prev /var/lib/nanovm
   mv /usr/local/bin/nanovm.prev /usr/local/bin/nanovm
   ```
3. Put back the old config if the upgrade changed it, start, and check as in
   Upgrade steps 4 and 5.

**Rolling back an app** is not this: move its tag back in the registry. The
proxy sees it within `tag_ttl_s` (30 s) plus the DNS TTL, and the next request
switches the worker (the old one drains). A `min=` floor in the TXT record
refuses versions below it. Restoring an object to an older version (Back up and
restore) is for bad data, not for a bad deploy.

## Back up and restore

**The backup is the capture store** (README, ["Backup and
restore"](../../README.md#backup-and-restore)): with `[capture] store` set, the
node ships every committed version of every object off the VM as it commits,
while it serves. There is nothing to schedule and nothing to stop; turn it on
before the first write (single-node guide, step 7). What a lost node loses is
what it hadn't shipped yet: `nanovm admin --config /etc/nanovm/nanovm.toml
backlog` says how much waits now.

Restoring runs on the running node, one object or one app, through `nanovm
admin` (as root on the host): `versions` to find a version, `restore <app>
<object> [<version>]` or `restore <app>`. A restored object answers 503 for the
seconds it takes; everything else keeps serving. Going back to an older version
makes a new version after the head, so nothing is lost. The steps for each case
(is the backup current, the store is down, an object's data is bad, a restore
refused, a crash during a restore, a lost node) are in [the recovery
runbook](../../notes/recovery-runbook.md).

A delete (`nanovm admin delete` / `delete-app`) removes the object's versions
from the store too: it can't be restored afterwards unless the store keeps its
own old versions, such as an S3 bucket's versioning (B-800).

## Disk full

What happens ([drills/disk-full.sh](drills/disk-full.sh), `data_dir` on a
32 MiB tmpfs filled to 0 by a file outside nanovm, on `beta/integration` with
objects and limits merged): below the node's `disk_reserve_mb` (here the
default, 1.6 MiB) every write that needs more space is refused before the disk
is, and the guest sees `insufficient-space` (the counter answers its own `507`,
its count unchanged); a request to an object that has no storage yet gets `507
disk-full` before any guest code runs, and makes nothing; what a request did
without needing space commits: the hostile guest's rewrite of a file (truncate,
then write) committed the truncation, as its write was refused and it answered
200 anyway, while its KV write failed. Reads keep working and return the last
committed state. The node's own commits go on: no `commit-failed`. Once space
is freed, writes resume with no step on your part: the counter went 3, (two
refused increments), 4, and the new object started at 1.

1. Confirm: `df -h /var/lib/nanovm`; `du -xh --max-depth=2 /var/lib/nanovm | sort -h | tail`.
2. Free space outside nanovm first (`journalctl --vacuum-size=…`, old backups,
   `/var/lib/nanovm.old`).
3. If that isn't enough: with the node stopped, `cache/` (compiled code) and
   `registry/` (downloaded apps) are caches: delete their contents and the node
   recompiles and refetches (and re-verifies) on demand. `tmp/` is per-instance
   scratch. Never touch an object's `journal` or `committed`; to drop an
   object or an app's data, use `nanovm admin delete` / `delete-app` (the
   three facts, above).
4. Start; writes recover object by object. Read back an object that failed.
5. Size for it: an object's quota counts logical bytes, and while a request runs
   its journal and replaced files can hold as much again (up to 3 × `disk_mb`
   per busy object, plus metadata); quotas don't cap the total. With `[capture]`,
   each object's history here adds up to `max_mb` (256 MiB by default): the
   whole volume every `snapshot_every` versions, what changed otherwise; with a
   `store`, what has shipped is removed here but the last version. Alert on
   filesystem usage and on `error.type = commit-failed`.

A single object answering `commit-failed` with `no storage space` while the
disk has room is a full capture history (`max_mb`), not a
full disk. With a `store`, versions wait here until they ship: check the store
is reachable (its credentials, its host) first. Otherwise raise the limits in
`[capture]` and restart; nanovm never drops versions to make room.

## Worker crash loop

Symptoms: one app answers `502 worker-down`; `journalctl -u nanovm` repeats
`nanovm: worker for app "…" exited: …` or `worker for app "…": worker did not
start (…): <last words>`. Other apps keep serving. A failed start is answered
from memory for 1 s (same digest and trust policy), then tried again on the
next request, so a broken app costs one spawn a second, not one per request.

1. Read the detail (`curl` the app, or the log line). By cause:
   - `entering the jail: …`: the jail can't be built. `setresuid … failed`:
     the supervisor lost `CAP_SETUID` (don't set `NoNewPrivileges=yes` on the
     unit). `… is not accessible to the worker`: `data_dir` or a parent isn't
     searchable by the app uid (keep it 0755, `UMask=0022`).
     `this kernel does not enforce Landlock`: Linux ≥ 5.13 with Landlock enabled,
     and the unit's `SystemCallFilter` must allow `landlock_*` and `seccomp`.
     `supervisor exited`: the supervisor died while the worker started.
   - `max_live_objects = … needs about … file descriptors`, `… memory
     mappings` or `… of address space`: raise `LimitNOFILE` or
     `vm.max_map_count` (`deploy/systemd/90-nanovm.conf`), remove `LimitAS`, or
     lower `max_live_objects` (`deploy/single-node/check-host.py` checks all
     three).
   - `exited: signal: 9 (SIGKILL)` with a `[worker] memory_mb` set: the cgroup's
     OOM killer; raise it or lower `max_live_objects`/`limits.memory_mb`.
   - `loading <ref>: not signed by a trusted identity`, `version … is below the
     minimum`, registry errors: a deployment problem; move the tag back.
2. Fix and send a request; the next start happens within a second.
3. Requests in flight when a worker died got `502 worker-down`; their writes
   rolled back unless they had committed. Clients retry with operation ids.

## Collector outage

Requests and commits don't wait for telemetry
([drills/collector-outage.sh](drills/collector-outage.sh): 20,000 warm GETs with
the Collector up, its port closed, or its address unroutable all returned 200 at
similar rates, 4.6–7.7k/s on a loaded 4-core VM). What is lost: spans and logs
produced while it is down (0 of 20,000 request spans arrived), except what is
still in the bounded export queues when it returns (180 of 3,000 in a separate
check); and the node's stderr gets one SDK warning per failed batch (about 30 a
second under that load: `OTLP export exhausted retries`,
`BatchLogProcessor.ExportError`).

1. Confirm: the Collector's health and its `otelcol_receiver_*` counters; the
   node's stderr warnings; request-rate panels dropping to no data while
   ingress logs show traffic.
2. Restart or fix the Collector or its backend. Nothing needs restarting on the
   node: exports resume with the next batch (checked: all 3,000 requests after
   the sink came back arrived).
3. Expect a gap in traces, metrics derived from spans, and logs; say so where
   SLOs are reported.
