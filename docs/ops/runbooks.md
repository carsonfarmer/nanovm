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
- **Never delete anything under `objects/*/~journal/`, `~committed/` or
  `~shipped/`.** A log in `~journal` is a request's undo record: the object
  recovers it on its next start, and deleting it can leave half a request's
  writes in place. `~committed` is capture's version history (`[capture]`, off
  by default): with a `store`, nanovm removes what it has shipped itself (all
  but the last version) and records it in `~shipped`. Don't prune either, and
  don't remove `[capture]` from a node that has history (its objects then
  refuse to start: `committed history requires capture to remain enabled`).
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
refuses versions below it. Restoring data from a backup is the last resort: it
loses every write after the snapshot.

## Back up and restore

[`scripts/backup.py`](../../scripts/backup.py) wraps restic; README "Backup and
restore" has the full contract. With the node stopped it locks every app
directory, backs up `objects/` (journals and capture history included; caches
left out), and tags only a complete snapshot.

```sh
export RESTIC_REPOSITORY=/srv/backups/nanovm RESTIC_PASSWORD_FILE=/root/restic-pass   # off-node in practice
restic init                                   # once
systemctl stop nanovm-proxy nanovm            # and keep them stopped
python3 scripts/backup.py backup /var/lib/nanovm --stopped     # prints the full snapshot id: record it
systemctl start nanovm nanovm-proxy
```

Restore into a root that doesn't exist, then swap it in:

```sh
systemctl stop nanovm-proxy nanovm
python3 scripts/backup.py restore <full snapshot id> /var/lib/nanovm.restored
mv /var/lib/nanovm /var/lib/nanovm.old && mv /var/lib/nanovm.restored /var/lib/nanovm
systemctl start nanovm      # same nanovm build, same jail_uid_base, same app identities
```

Read and write an object before opening ingress; keep `/var/lib/nanovm.old`
until you are sure.

With a capture `store` (single-node guide, step 7), committed versions also
ship off the VM as they happen; `nanovm::ship::restore` rebuilds an object from
the store on a fresh root.

> **Placeholder (`beta/recovery`):** restoring one app or one object on a
> running node, from the store or a backup, with `nanovm admin`. Filled in when
> `beta/recovery` is integrated. [drills/backup-restore.sh](drills/backup-restore.sh): count 3
and a KV value before; after restoring into a fresh root, the same values, app
uids 200000/200001 preserved, and the next write gives 4. Run `restic check
--read-data` on a schedule; a backup you haven't restored is a hope.

## Disk full

What happens ([drills/disk-full.sh](drills/disk-full.sh), `data_dir` on a full
32 MiB tmpfs, on round 3): a write that needs new space fails in the guest,
which answers as it chooses (the counter's own `507`, its count unchanged); a
request whose object has no journal yet (its first write) answers
`500 commit-failed` (`journal requires recovery`) and rolls back; what a request
wrote without needing space commits (each object keeps its journal's log, so
an overwrite in place still did, while its KV write in the same request
failed). Reads keep working and return the last committed state. Once space is
freed, writes resume with no step on your part: the counter went 3, (two
refused increments), 4, and the new object started at 1.

1. Confirm: `df -h /var/lib/nanovm`; `du -xh --max-depth=2 /var/lib/nanovm | sort -h | tail`.
2. Free space outside nanovm first (`journalctl --vacuum-size=…`, old backups,
   `/var/lib/nanovm.old`).
3. If that isn't enough: with the node stopped, `cache/` (compiled code) and
   `registry/` (downloaded apps) are caches: delete their contents and the node
   recompiles and refetches (and re-verifies) on demand. `tmp/` is per-instance
   scratch. Never touch `objects/*/~journal/` or `objects/*/~committed/`.
4. Start; writes recover object by object. Read back an object that failed.
5. Size for it: an object's quota counts logical bytes, and while a request runs
   its journal and replaced files can hold as much again (up to 3 × `disk_mb`
   per busy object, plus metadata); quotas don't cap the total. With `[capture]`,
   each object's history here adds up to `max_mb` (256 MiB by default): the
   whole volume every `snapshot_every` versions, what changed otherwise; with a
   `store`, what has shipped is removed here but the last version. Alert on
   filesystem usage and on `error.type = commit-failed`.

A single object answering `commit-failed` with `no storage space` while the
disk has room is a full capture history (`max_versions` or `max_mb`), not a
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
