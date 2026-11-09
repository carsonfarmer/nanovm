# Ops notes

Operations and packaging work from IDEAS.md (sections E, F, L, M), one branch per
item, each from `cf/main-next`. Plain language; numbers and how to reproduce.

## L-115 runbooks (`ideas/runbooks`)

**What:** `docs/ops/runbooks.md`: upgrade, rollback (nanovm and app), backup and
restore with `scripts/backup.py`, disk full, worker crash loop, Collector outage,
for the systemd single-node layout. `docs/ops/drills/` has four scripts that
reproduce the behaviour each runbook quotes.

**Drills (dev builds, this sandbox):**
- `backup-restore.sh` (restic 0.16.4, as root): count 3 and KV value `kept`
  before; backup, move the root away, restore into a fresh root: same values,
  app uids 200000/200001 preserved, next write gives 4.
- `disk-full.sh` (data_dir on a full 32 MiB tmpfs): every write, to existing or
  new objects, `500 commit-failed` ("journal requires recovery"); reads return the
  last committed state; two logs left in `~journal`; after freeing space each
  object recovered on its next request (counter 3 → 4, failed KV write gone, no
  logs left).
- `binary-swap.sh`: `cp` over the running binary fails (text file busy);
  `install` succeeds, the running app keeps serving, a new app's worker fails
  `worker spec: invalid length 3, expected a tuple of size 4` (old supervisor, new
  worker after N-551). Recorded as N-552 (open; fix sketched).
- `collector-outage.sh`: 20,000 warm GETs per variant, two rounds, Collector up
  / port closed / address unroutable: all 200 (except one 500 and one 502 at load
  average ~40, in the "blackhole" and "up" variants), 4.6–7.7k/s with no pattern
  by variant; 0 spans during an outage, ~750–810 stderr warnings per run. A
  separate check: after the sink came back, 180 of 3,000 earlier spans (still
  queued) and all 3,000 later ones arrived.

**Checked against the code:** journal path and recovery at instance start
(`journal.rs`, `quota.rs:75`), 1 s failed-start memory (`RESTART_BACKOFF`),
worker exit and last-words log lines, preflight messages, caches vs state
under `data_dir`, deny-unknown-fields config, tag TTL and version floor.

**After the base moved to `3dafe33` (W-9 local capture):** the runbooks now
cover `~committed/` (never prune; never drop `[capture]` from a root with
history; no rollback past capture support), capture's storage in sizing, and a
full capture history (`commit-failed` "no storage space" on one object while the
disk has room), each checked with `docs/ops/errors-check.sh` on a dev build of
`3dafe33`.
