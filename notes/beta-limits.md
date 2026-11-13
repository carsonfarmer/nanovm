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
seccomp allowlist) before any growth and any new entry: below `disk_reserve_mb` free (`f_bavail`,
what the worker's uid may use), those are refused with `insufficient-space`, while writes in place,
truncation, removal, rollback and commit go on. Each refusal is an `fs-denied` event naming its
cause: `the app's N MiB disk budget is full`, `the node's disk is below its N MiB reserve`, the
object's own quota as before, and now `the node's disk is full` for an ENOSPC the journal meets
(before, that was reported as the object's quota).

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
places workers in `<own>/workers/worker-<n>` as before. Under systemd that needs only `Delegate=yes`
(the ops drop-in's `ExecStartPre` and `cgroup` path are no longer needed). Where it can't (no cgroup
v2 with those controllers, a cgroup it doesn't own, other processes in it), the node refuses to start
and says to set `cgroup = "off"` or name a delegated directory: fail closed (owner question 3).

**Not verified here:** this sandbox gives the memory, cpu and pids controllers to cgroup v1 (its
cgroup2 at `/sys/fs/cgroup/unified` has none), so the delegated branch can't run on it; the refusal
does. `scripts/vm-test.sh` now includes the test, which checks the placement and the default
`memory.max` and `cpu.max` in a VM with cgroup v2 (not run for this branch: see below).

## Evidence

All on this sandbox (4 vCPU, shared with several other agents' builds and tests throughout, load
average 5–10), as root. Runtime lines (CI's count): 7,410 → 7,728 (+318: `gate.rs` 145, `quota.rs`
+98, `supervisor.rs` +43, `config.rs` +19, `proxy.rs` +7, the rest +6).

### Tests

New, each a hostile test of one limit:

| Test | What it does |
|---|---|
| `clients_past_their_rate_are_refused_however_they_forward` | proxy at 1 request/s per client: 10 at once from an untrusted peer, each with a different forged `X-Forwarded-For`, are 1–3 served and the rest 429 with `Retry-After`; behind a trusted peer, a client is limited by its own address, and four forgeries written left of it (another address, the terminator's, junk) are all refused; another client is served; the terminator's own requests never are refused; a node reached directly refuses past its rate too |
| `apps_past_their_rate_are_refused` | one app at 1 request/s: 10 clients at once share it; another app is served |
| `connections_are_capped_per_client_and_in_all` | a third connection from one address gets 503 `too-many-connections` with `Retry-After: 1`, the other two are served, and a closed one frees its slot; with 2 in all, a third waits until one closes, then is served |
| `a_full_app_budget_refuses_only_that_apps_writes` | 8 MiB tmpfs, 3 MiB budget, jailed workers: app A writes 2 MiB in one object and is cut at exactly 1 MiB in another (`insufficient-space`, `fs-denied` "the app's 3 MiB disk budget is full"); app B then writes 2 MiB; A deletes and writes again |
| `the_disk_reserve_keeps_writes_committing` | 8 MiB tmpfs, 3 MiB reserve, budget above the disk: one object fills the disk to the reserve (`fs-denied` "below its 3 MiB reserve"); 20 counter objects' increments are each a clean 507, never `500 commit-failed`; an overwrite in place commits; after a delete, increments work again |
| `workers_run_under_host_sized_limits_or_the_node_refuses_to_start` | `cgroup = "auto"` here refuses to start, naming `cgroup = "off"`; where cgroup v2 can be delegated, checks the supervisor moved to `<own>/supervisor`, the worker is in `<own>/workers/…` and its `memory.max`/`cpu.max` are the defaults |
| unit: `clients_are_named_only_by_trusted_forwarders`, `rates_are_per_client_and_per_app`, `connections_are_capped_per_client`, `trusted_takes_addresses_and_ranges` | the gate's rules, IPv6 /64, IPv4-mapped addresses |
| unit: `objects_share_their_apps_budget`, `below_the_reserve_only_growth_is_refused` | the budget across objects (cut short, refused, freed), journals not counted, `/tmp` released at instance end, an uncommitted removal counted until rollback; growth and entries refused below the reserve while overwrites and shrinking go on |
| unit: `worker_limits_are_sized_from_the_host` | the defaults, and 0 still unsets |
