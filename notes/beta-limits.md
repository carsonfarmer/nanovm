# Beta limits: rate limits, connection caps, disk budgets, worker caps

The production audit found no abuse limits anywhere: no rate limits or connection caps at the
proxy or a node's own listener; no disk budget per app or per node (on an 8 MiB root about 560
counter objects filled the disk, after which every write on the node failed `500 commit-failed`);
and workers ran without cgroup limits by default (`cgroup` unset, `[worker]` limits 0), so one app
could touch `memory_mb × max_live_objects` (64 GiB) and every core. This branch (`beta/limits`)
adds all three. ISSUES: B-301 to B-306.

## 1. Clients: rate limits and connection caps (`gate.rs`, `[clients]`)

Both public listeners, the proxy's and a node's `listen`, go through one `Gate`:

| Limit | Default | Past it |
|---|---|---|
| `rate`: requests a second from one client address (an IPv6 /64), bursts of twice that | 100 | `429 rate-limited`, `Retry-After` |
| `app_rate`: requests a second to one app, bursts of twice that | 1,000 | `429 rate-limited`, `Retry-After` |
| `connections_per_client` | 256 | `503 too-many-connections`, `Retry-After: 1`, then closed |
| `connections` in all | 10,000 | new connections wait in the listen backlog |
| `trusted`: forwarders (addresses or CIDR ranges) | loopback | — |

**Library.** `governor` 0.10 (GCRA, keyed). Built with `std` only, not its `dashmap`, `quanta` or
`jitter` features: four new crates (`governor`, `nonzero_ext`, `spinning_top`, `futures-timer`)
instead of six, one parking_lot mutex per limiter rather than a sharded map (two hash lookups a
request). Tower's limiters (`RateLimit`, `ConcurrencyLimit`) aren't keyed, and they apply
backpressure rather than refuse; `tower_governor` trusts `X-Forwarded-For` from anyone by default
and is built for tower services, which nanovm's listeners aren't. Connection caps are a semaphore
and a map of open connections per address, which needed no library. Keys whose limit has refilled
are dropped every 10 s (`retain_recent`, `shrink_to_fit`), so the maps hold only clients active in
the last few seconds.

**Who the client is.** A connection's peer, unless the peer is in `trusted`: then the last address in
`X-Forwarded-For` that isn't trusted. A client can only write to the left of the address its
terminator appends, which is never read. A hop that doesn't parse counts as the trusted hop that sent
it (fail closed: that terminator's traffic shares one limit). Trusted peers' own requests and
connections aren't limited: that is what lets the proxy, on loopback, send everything to a node
without the node limiting it as one client, and what a terminator's health checks need. A
terminator that doesn't set `X-Forwarded-For` therefore leaves its traffic unlimited; Caddy sets it.

## 2. Disk: per-app budget and node reserve (`quota.rs`, `app_disk_mb`, `disk_reserve_mb`)

Built on the per-object accounting rather than beside it: each instance's `QuotaGuard` already
counts the logical bytes of its object's files, and every change of that count now goes through one
function (`set_bytes`) that also moves the app's total (`quota::Disk`, one per app per worker).
Growth (a write past a file's end, a `set_size` up) first takes bytes from the app's budget with a
compare-and-swap, so concurrent objects of one app can't overshoot it; what a write doesn't use is
given back. A worker measures every volume of its app when it loads; an object's instance replaces
its entry with what it finds when it starts, and when it ends its volume is measured again (so what
it held in `/tmp` is released), plus what an uncommitted removal would put back on rollback. The
instance's last reference is dropped before its volume's lock is, so a new instance never starts
before the old one's usage is settled.

The reserve is `fstatvfs` on the app's `objects/` directory (a new `fstatfs` in the worker's
seccomp allowlist) before any growth and any new entry: below `disk_reserve_mb` free (default a
twentieth of the filesystem, at most 1 GiB, so a small disk isn't all reserve; `f_bavail`,
what the worker's uid may use), those are refused with `insufficient-space`, while writes in place,
truncation, removal, rollback and commit go on. Each refusal is an `fs-denied` event naming its
cause: `the app's N MiB disk budget is full`, `the node's disk is below its N MiB reserve`, the
object's own quota as before, and now `the node's disk is full` for an ENOSPC the journal meets
(before, that was reported as the object's quota).

With `beta/objects` merged, an app's objects are `objects/<app>/<shard>/<id>/data`, an object has
a directory only from its first change, and its journal rolls back what an instance left uncommitted
when the instance ends: the budget walks each `<shard>/<id>/data` at load, and an ended instance's
volume is measured after that rollback (a `Park` field dropped after the journal), so the app counts
exactly what stays; a volume the journal removed because it emptied counts 0.

Both limits count logical bytes, as the object quota does. Journals (68 KiB per object between
requests), the capture outbox and block rounding are on top (B-304); the objects cap on
`beta/objects` bounds how many objects there are, and the reserve keeps the disk from reaching zero.

## 3. Workers: cgroup v2 on by default, host-sized caps (`config.rs`, `supervisor.rs`)

`[worker]` now defaults to `memory_mb` = a quarter of the host's memory and `cpus` = all its cores
but one (one on a one-core host); `pids` stays unset (a worker can only start threads, which tokio
bounds). `cgroup` is now a string: `"auto"` (default), a directory, or `"off"`. With `"auto"` the
supervisor reads its own cgroup v2 from `/proc/self/cgroup` and `/proc/self/mountinfo`, moves itself
into `<own>/supervisor` (cgroup v2 keeps processes out of a group that hands controllers to its
children), enables the controllers its limits need for `<own>`'s and `<own>/workers`' children, and
places workers in `<own>/workers/worker-<n>` as before, through round 3's `Cgroup::take` (which
refuses a directory that isn't cgroup v2, holds it for this node alone, and removes only an earlier
run's `worker-<n>`), as for a directory named in `cgroup`. Under systemd that needs only `Delegate=yes` and
`ProtectControlGroups=no` (the ops drop-in's `ExecStartPre` and `cgroup` path are no longer needed). Where it can't (no cgroup
v2 with those controllers, a cgroup it doesn't own, other processes in it), the node refuses to start
and says to set `cgroup = "off"` or name a delegated directory: fail closed (owner question 1).

**Not verified here:** this sandbox gives the memory, cpu and pids controllers to cgroup v1 (its
cgroup2 at `/sys/fs/cgroup/unified` has none), so the delegated branch can't run on it; the refusal
does. `scripts/vm-test.sh` now includes the test, which checks the placement and the default
`memory.max` and `cpu.max` in a VM with cgroup v2 (not run for this branch: see below).

## Evidence

All on this sandbox (4 vCPU, 16 GiB, ext4), as root, shared with several other agents' builds and
tests: load average 5–30 most of the time, near 0 for one short window. Runtime lines (CI's count):
this branch merges `beta/objects` (its object layout, `objects/<app>/<shard>/<id>/data`, and per-app
object cap), as the coordinator asked, and `cf/round3` at b12ffba (the worker fixes), so its delta is
counted against the two merged: 7,717 → 8,044 (+327: `gate.rs` 145, `quota.rs` +112, `supervisor.rs`
+43, `config.rs` +19, `proxy.rs` +7, the rest +1).
Before that merge, against `cf/round3`: 7,410 → 7,728 (+318).

The measurements below were taken before the merge (the hot path they cover, the gate and the
write path's checks, didn't change in it).

### CPU per request (the hot path)

Method as AGENTS.md: release builds (`--features dev`) of `cf/round3` (`old`) and this branch
(`new`) side by side, each a supervisor with one jailed counter worker on loopback (no cgroup),
oha against 100 objects, process CPU of the supervisor and worker from `/proc` per request,
alternating old/new each round and reversing the order every other round. `new` runs the gate in
its most expensive form: `trusted = []` (so every request is a limited client) and rates of 10⁹/s
(checked on every request, never reached): an extension lookup, the `X-Forwarded-For` scan, two
keyed GCRA checks. Writes (`POST /incr`) also take the reserve's `fstatfs` twice (the new temporary
file and its growth) and the app budget's compare-and-swap.

| Run | old: median [range] µs/request | new | Machine |
|---|---|---|---|
| GET, 1 client, 5 rounds × 20,000 | 150.5 [138.0–266.0] | 174.0 [138.0–196.5] | load near 0, rising in rounds 1–3 |
| — of those, rounds 4 and 5 | 138.0, 140.0 | 138.0, 141.5 | idle |
| GET, 64 clients, 5 × 100,000 | 74.8 [68.6–79.8] | 77.1 [71.0–79.9] | idle |
| — supervisor only (where the gate runs) | 21.4 [19.4–22.4] | 21.8 [20.7–23.2] | idle |
| GET, 64 clients, 5 × 100,000 | 111.4 [101.6–113.7] | 105.5 [102.8–111.5] | load ~20 |
| POST /incr, 64 clients, 5 × 40,000 | 1,021.8 [949.0–1,052.5] | 1,037.8 [1,005.5–1,066.5] | load ~20 |
| POST /incr, 64 clients, 3 × 20,000 | 844.5 [622.5–909.0] | 864.5 [852.5–898.0] | load ~30 |

No difference stands out of the noise: every pair of ranges overlaps, the medians move both ways,
and where they moved most (GET, one client, the first three rounds and a later loaded rerun with
old 189 vs new 281) the worker's CPU moved as much as the supervisor's although nothing in the
worker's read path changed, so it was the machine. In the two rounds taken while it was idle, old
and new are within 1.5 µs. For scale, `fstatvfs` costs 213 ns of CPU on this ext4 (a C loop of 10⁶
calls), against about 1 ms of CPU per committed write.

### Worker start (the app's disk is measured when it loads)

`nanovm run` (in-process node) until it is ready, with N objects of 5 files of 100 bytes each,
old/new alternating, the first run of each (compiling) dropped, machine loaded (load ~25):
no objects 40–57 ms both; 2,000 objects (10,000 files) old 17–30 ms, new 42–118 ms; 10,000 objects
(50,000 files) old 28–82 ms, new 153–277 ms. So the walk costs roughly 3–4 µs a file, warm, on every
worker start (an app's first request, a version switch, a restart). The objects cap bounds it; a
walk in the background with a short window of soft budget is the alternative (B-307).

### Tests

New, each a hostile test of one limit:

| Test | What it does |
|---|---|
| `clients_past_their_rate_are_refused_however_they_forward` | proxy at 1 request/s per client: 10 at once from an untrusted peer, each with a different forged `X-Forwarded-For`, are 1–3 served and the rest 429 with `Retry-After`; behind a trusted peer, a client is limited by its own address, and four forgeries written left of it (another address, the terminator's, junk) are all refused; another client is served; the terminator's own requests never are refused; a node reached directly refuses past its rate too |
| `apps_past_their_rate_are_refused` | one app at 1 request/s: 10 clients at once share it; another app is served |
| `connections_are_capped_per_client_and_in_all` | a third connection from one address gets 503 `too-many-connections` with `Retry-After: 1`, the other two are served, and a closed one frees its slot; with 2 in all, a third waits until one closes, then is served |
| `a_full_app_budget_refuses_only_that_apps_writes` | 8 MiB tmpfs, 3 MiB budget, objects of 2 MiB quota, jailed workers: app A writes 2 MiB in one object and is cut at exactly 1 MiB in another (`insufficient-space`, `fs-denied` "the app's 3 MiB disk budget is full"); app B then writes 2 MiB; A deletes and writes again |
| `the_disk_reserve_keeps_writes_committing` | 8 MiB tmpfs, 3 MiB reserve, budget above the disk: objects of 2 MiB fill the disk until one is refused, by the reserve (`fs-denied` "below its 3.0 MiB reserve"), not by the objects branch's new-object check; 20 counter objects' increments are each a clean 507, never `500 commit-failed`; an overwrite in place commits; after a delete, increments work again |
| `workers_run_under_host_sized_limits_or_the_node_refuses_to_start` | `cgroup = "auto"` here refuses to start, naming `cgroup = "off"`; where cgroup v2 can be delegated, checks the supervisor moved to `<own>/supervisor`, the worker is in `<own>/workers/…` and its `memory.max`/`cpu.max` are the defaults |
| unit: `clients_are_named_only_by_trusted_forwarders`, `rates_are_per_client_and_per_app`, `connections_are_capped_per_client`, `trusted_takes_addresses_and_ranges` | the gate's rules, IPv6 /64, IPv4-mapped addresses |
| unit: `objects_share_their_apps_budget`, `below_the_reserve_only_growth_is_refused` | the budget across objects (cut short, refused, freed), journals not counted, `/tmp` released at instance end, an uncommitted removal counted until rollback; growth and entries refused below the reserve while overwrites and shrinking go on |
| unit: `worker_limits_are_sized_from_the_host` | the defaults, and 0 still unsets |

### Without the fixes

- **The audit's run** (`nanovm run` counter on an 8 MiB tmpfs, before this branch): about 560
  objects' first increments succeeded, then every write on the node failed `500 commit-failed`, and
  the event blamed the object's own quota ("64 MiB disk quota full").
- **Without the reserve** (mutation M6 below, the message check skipped): the filler takes the tmpfs
  to zero, every counter increment then gets `io` from its write (ENOSPC), and no `fs-denied` event
  says why. With it: a clean `insufficient-space` 507, and the event names the reserve.
- **Without the app budget** (M5): app A's second object writes its full 2 MiB past the 3 MiB budget.

### Mutation checks

Each breaks one mechanism, runs the tests that should catch it, then `git checkout --` the file and
checks `git diff` is empty (all 13 were). Run before the `beta/objects` merge.

| # | Mutation | Caught by |
|---|---|---|
| M1 | trust `X-Forwarded-For` from any peer | `clients_are_named_only_by_trusted_forwarders`, `clients_past_their_rate_are_refused_however_they_forward` |
| M2 | no per-client rate | `rates_are_per_client_and_per_app`, `clients_past_their_rate_…` |
| M3 | no per-client connection cap | `connections_are_capped_per_client`, `connections_are_capped_per_client_and_in_all` |
| M4 | no total connection cap | `connections_are_capped_per_client_and_in_all` |
| M5 | grant growth past the app's budget | `objects_share_their_apps_budget`, `a_full_app_budget_refuses_only_that_apps_writes` |
| M6 | never below the reserve | `below_the_reserve_only_growth_is_refused`, `the_disk_reserve_keeps_writes_committing` |
| M7 | don't re-measure an ended instance's volume | `objects_share_their_apps_budget` |
| M8 | forget an uncommitted removal at instance end | `objects_share_their_apps_budget` |
| M9 | no `fstatfs` in the jail | `a_full_app_budget_refuses_only_that_apps_writes` |
| M10 | `cgroup = "auto"` runs without cgroups | `workers_run_under_host_sized_limits_or_the_node_refuses_to_start` |
| M11 | `[worker] memory_mb` defaults to 0 | `worker_limits_are_sized_from_the_host` |
| M12 | new entries ignore the reserve | `below_the_reserve_only_growth_is_refused` |
| M13 | IPv6 clients by address, not /64 | `clients_are_named_only_by_trusted_forwarders`, `connections_are_capped_per_client` |

M8 was for the pre-merge design (the removal added back at measuring time); after the merge the
journal rolls it back first, and the same test checks the count (`objects_share_their_apps_budget`,
now on objects-backed journals in the sharded layout).

### Not done or not verified

- **cgroup v2 here:** not writable with controllers (they are on cgroup v1), so only the refusal is
  tested; `scripts/vm-test.sh` (QEMU, no KVM here) wasn't run: it needs a release build of the
  integration tests, which the shared disk (3–8 GB free) couldn't spare.
- **W-35 / N-182** (a global budget for what the proxy buffers): not taken; they bound memory, not
  requests or disk, and this branch is already +327 lines.
- **Four example tests** (`gateway`, `oauth`) didn't run: their apps need `wac` to build, which isn't
  installed here, and the other worktrees' builds of them differ.
- **The proxy's CPU per request** wasn't measured separately: it runs the same gate code as the node's
  front end, which was.

## Owner questions

1. **Without a cgroup subtree, refuse to start (built) or warn and run?** Recommended: refuse. Running
   without caps is the audit's gap; `cgroup = "off"` is the explicit way to accept it (dev, containers
   without a writable cgroup v2).
2. **Client limit defaults** (100 requests/s per address, 1,000/s per app, bursts of twice that, 256
   connections per address, 10,000 in all): recommended as they are for a beta of a few developers,
   raised per node when an app needs more.
3. **Trust loopback as a forwarder by default?** Recommended: yes. Caddy and the proxy sit on loopback
   in the documented setup, and a local process can already reach a node directly; the alternative
   limits everyone behind a local terminator as one client.
4. **Disk defaults** (an app may hold a quarter of the data filesystem; a twentieth, at most 1 GiB, is
   kept free): recommended, with `app_disk_mb` set explicitly once nodes have a known size.
5. **Worker defaults** (a quarter of the host's memory, all cores but one): recommended; fair sharing
   between workers under contention comes from cgroup v2's equal weights anyway.

