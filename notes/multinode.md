# Many nodes (W-10): placement, leases with epochs, fenced commits

Branch `cf/multinode`. nanovm ran one proxy in front of one node, so an
object had one live instance only because there was one node. This makes it
run on several, keeping that guarantee with leases rather than with luck.

## What it does

**Placement (proxy).** `ProxyConfig.nodes` lists the nodes (`host:port`,
replacing `node`). Each `(app, object)` has a home by rendezvous hashing:
every node's SHA-256 score of `node \0 app \0 object`, highest first
(`proxy::rank`). Adding or removing a node moves only the objects that node
gains or loses. The proxy sends a request to the object's home; a node that
doesn't hold the object answers **421** with `nanovm-owner` naming the holder
(or empty), and the proxy tries that node, else the next in the ranking, past
nodes it can't connect to (1 s connect timeout), sleeping 100 ms after each
full round, until `failover_ms` (30 s) passes (then 503). Neither refusal ran
the request, so retrying is safe without request ids. With more than one node
the proxy buffers request bodies (up to `max_request_body_kb`) so it can send
them again; with one node it streams as before and never retries.
WebSocket events and calls between objects take the same path.

**Leases (`lease.rs`).** With `leases = "s3://bucket/prefix"` on the nodes,
the leases live in an object store, through the `object_store` crate (S3 and
S3-compatibles such as R2 and MinIO; credentials from `AWS_*` variables). Its
in-memory store is the test fake. Every write is conditional (`If-None-Match:
*` to create, `If-Match` to update), so of two nodes racing one wins.
- `objects/<app>/<object>` is `{node, session, epoch}`. Taking it writes
  `epoch + 1`, conditionally on what was read, so epochs only ever rise,
  across all nodes. Releasing writes no node, same epoch.
- `nodes/<node>` is `{session, beat}`: one record per node renews all its
  leases (one conditional PUT every `lease_ms / 4`, 0.4/s at the default
  10 s), not one per object.
- A node's leases hold until 7/8 of `lease_ms` after it **sent** its last
  renewal that succeeded. After that its workers serve none of those objects
  and commit nothing; the supervisor then starts a new session (`session + 1`).
- Another node takes an object whose holder's session record it has seen
  unchanged for a whole `lease_ms`, by its own monotonic clock. So clocks need
  not agree, only tick at about the same rate (the 1/8 margin). Before taking
  the object it **ends** that session with a conditional write
  (`session + 1`), so that a renewal already on its way can't land afterwards
  and extend the holder's leases. Without that write a takeover and a late
  renewal could both succeed (`a_late_renewal_cannot_outlive_a_takeover`).
- Without a store (`leases = ""`) there is one node and every lease is its
  own at epoch 0: no store, no session, no extra work per request.

Why it is safe: B's takeover write at time k succeeded only if A's session
record was still at the version B had seen for `lease_ms`. So A's last
successful renewal was sent at least `lease_ms` before k, and A's leases ended
`lease_ms / 8` before k. Every later renewal by A fails its condition.

**Nodes (supervisor + worker).** The supervisor owns the store client and
the session. It renews the session and tells each live worker its session id
and `until` (CLOCK_MONOTONIC, the same in both processes). A worker's actor
asks its supervisor for the object's lease (`POST http://nanovm/_lease/{id}`
on the egress link; guests are denied that path) **before instantiating**,
and gives it up when the actor exits. A worker:
- refuses a request with 421 when the object's lease is lost, when queueing
  it; a request already queued, or running, when the lease goes fails its
  commit, which resets the instance, and the next is refused (421) when the
  actor asks for the lease again before a new instance;
- evicts idle objects at once when the session lapses or changes; busy ones
  stop at their next request or commit;
- refuses any commit once the lease is gone (`ObjShared::lease_epoch`, below);
- strips `nanovm-owner` from guest responses. Otherwise a guest answering 421
  could make the proxy run its request again on every node for `failover_ms`.

**Calls between objects.** With leases, a call (`http://nanovm/objects/{id}/…`)
goes from the worker to its supervisor, which names the app (`nanovm-app` with
the app's name, and `nanovm-host`) and sends the call to the proxy's private
endpoint (`NodeConfig.proxy`, which replaced `publish_to`: publishes go to its
`/publish`, calls to its `/objects/…`). The proxy resolves the host as it does
for socket events, refuses the call if the host now serves another app, and
sends it to the object's node. Nodes stay unaware of each other (Q49). Calls
carry `nanovm-callers: a,b,c`, the objects waiting on them, set only by the
node (the proxy strips client `nanovm-*` headers). The cycle check then refuses
(508) a call to an object that waits, through recorded waits, on the caller
**or any of its callers**, and a node receiving a call refuses it the same
way. This catches every cycle along a chain of calls, and every cycle whose
waits are recorded on one node. One that closes through two waits recorded on
other nodes, not along one chain, is ended by the request timeout instead. The
limits are unchanged: the caller's outbound permits and request-body cap, and
the callee's own limits on its node. One node keeps the in-process path. Trace
context crosses nodes as `traceparent`.

## The hook, and W-9's capture

`ObjShared::lease_epoch() -> io::Result<u64>`: the epoch of the node's lease on
the object, or an error once the node no longer holds it (0 with one node). It
is reachable from the journal, which holds the object's `ObjShared`, so it
sits where W-9's local capture (`committed.rs`, merged from `cf/main-next`
3dafe33) decides a commit:

```rust
// journal.rs, Journal::commit: after the syncs and committed::prepare
self.obj.lease_epoch()?;                     // the decision: only under the lease
std::fs::remove_file(self.dir.join(LOG))?;   // deleting the log is the commit
```

- `committed::prepare` records the epoch in the captured version
  (`Version.format` 2, field `epoch`, next to `sequence`: no second
  sequence). A version is named `(app, object, epoch, sequence)`; epochs rise
  strictly per object across nodes.
- A lease that lapses while the version is written (a full-volume archive,
  which can take a while) refuses the commit at the decision. The journal is
  left poisoned; recovery discards the staged version and undoes the change,
  as for any failure before the decision.
- A request that changed nothing still asks the journal, which checks the
  lease then too, so a node that lost the object publishes nothing for it.

What remains for W-9's stream to make this a fence off the node (W-90):

1. **The local check is not the fence.** It stops a node that knows it lost
   the lease. A node that doesn't know yet (a long pause between the check and
   the unlink) can still commit locally. That is harmless only if the version
   can't become durable off the node: **the write that ships a version must be
   conditional on the lease record not having moved**. Proposed: keep the
   stream's head in the lease record (`objects/<app>/<object>` gains `head`),
   written `If-Match` the version that the grant (or the previous shipped
   commit) produced. A 412 means another node took the object, so the version
   is refused. `Grant` already carries that version (`e_tag`, `version`). The
   supervisor, which holds the store client, would do the write when the
   worker ships the version: one store round trip per commit, which remote
   durability needs anyway.
2. **Takeover.** B gets epoch e+1 only after ending A's session. Before
   serving, B must restore the object up to the head recorded in the lease
   record it overwrote. Until the stream exists, the failover test shares one
   data root between the nodes, standing in for it.
3. **Never delete lease records**: an object's epochs would restart.

## Tests (all hostile; each mutation-checked)

Unit (`lease.rs`, in-memory store; `journal.rs`; `proxy.rs`):
- `leases_pass_from_node_to_node_at_rising_epochs`: grant at epoch 1, refusal
  naming the holder, release then take at 2, a stale release changes nothing,
  a takeover only after a whole lease unchanged (3), re-take (4).
- `a_renewing_holder_keeps_its_leases_and_a_lapsed_one_loses_them_first`: a
  renewing holder is never taken over (3 leases long). One gone quiet has
  lapsed before the takeover, which comes at least `lease_ms` after its last
  renewal.
- `a_late_renewal_cannot_outlive_a_takeover`: a renewal sent before a takeover
  and landing after it fails.
- `commits_are_decided_only_under_the_lease`: a captured version records the
  epoch (7); with the lease lapsed, the journal's commit fails at the decision
  and recovery undoes the change, and a commit of nothing fails too (so its
  publishes don't go out).
- `rendezvous_moves_only_what_it_must`: 4 nodes, 4,000 objects: each node is
  home to 800–1,200; dropping a node moves only its objects; the list's order
  doesn't matter.

Integration (`tests/integration.rs`, `mod nodes`; real jailed workers, two
in-process supervisors, a proxy, a shared in-memory lease store):
- `an_object_moves_to_another_node_when_its_node_stops`: A serves and commits
  `one`. A stops: its own runtime is dropped, killing its server, renewals and
  workers. B serves `one` (shared data root) at epoch 2, no sooner than
  `lease_ms` later.
- `a_node_that_lost_an_objects_lease_cannot_commit_it`: A answers and works on
  after answering (the test-only probe). A is then cut off from the store.
  Within `lease_ms`, A evicts its idle objects and refuses the busy one. B takes
  it at epoch 2 and serves it, including a request that had queued on A before
  the cut. A's late post-return work runs (it calls an object on B through the
  proxy and writes), and its commit is refused ("no longer holds the object's
  lease"): the write stays uncommitted in A's journal, to be rolled back.
- `calls_reach_objects_on_other_nodes_and_cycles_across_nodes_are_refused`:
  x on A calls y on B, which commits on B. x → y → x is refused (508) at once
  (< 1 s; without the chain it waits out the 1 s request timeout). B, renewing,
  keeps y at epoch 1 across 3 leases.
- `a_guest_cannot_pose_as_a_node_refusing_its_object`: a guest's own 421 with
  `nanovm-owner` reaches the client as-is and runs once. Once idle, the
  object's lease is released.
- `socket_events_reach_their_objects_node`: a WebSocket opened through the
  proxy echoes from its object's node, which alone holds the lease.
- The capture test `sqlite_captures_recover_without_the_original_node` (opt-in
  SQL suite) now also checks that a single node's versions carry epoch 0. It
  passes, as do `sqlite_composed_pool_capacity` and `sqlite_composed_provider`,
  with the composed SQLite built locally (`sqlite_composed_recovery_cuts`
  needs the test-hooks build, not built here).

**Caller identity (W-602, W-12).** `nanovm-callers` names, last, the object
making the call. Only nodes set it: the worker inserts it on every call,
replacing anything the guest put there; the proxy strips every `nanovm-*`
header from clients and keeps only `nanovm-callers` on its private call route;
and on one node the call never leaves the worker. So an object can serve an
internal route only to its app's objects by checking that the header is
present, and to one caller by checking its last entry. That is W-12's trusted
caller header, and it would close W-602. It holds within the existing trust
model: a node reached without the proxy trusts what it is sent, as it does
`nanovm-app`. Before relying on it: a test with a guest that echoes the header
(a client's forged header never arrives; a guest's own is replaced), and README
text. Post-return work, which no caller waits on, sends only its own id.

Mutation checks (each removes one mechanism; the named test must then fail; all
restored with `git checkout`, empty diff after). Runs on the merged tree:

| # | removed | caught by |
|---|---|---|
| M1a | the journal's lease check at the decision | `commits_are_decided_only_under_the_lease` |
| M1b | the epoch in the captured version | same |
| M1d | every lease check on commit | `a_node_that_lost_an_objects_lease_cannot_commit_it` |
| M1e | the check on a commit of nothing | `commits_are_decided_only_under_the_lease` |
| M2 | refusing a lost object's request at enqueue | `a_node_that_lost…` (it hangs behind A's work) |
| M4 | waiting a whole lease before a takeover | `leases_pass…`, `an_object_moves…` |
| M5 | ending the session before a takeover | `a_late_renewal_cannot_outlive_a_takeover` |
| M6 | raising the epoch | `leases_pass…`, `an_object_moves…` |
| M7 | checking the holder's session | `leases_pass…` |
| M8 | passing over an unreachable node | `an_object_moves…` |
| M9 | retrying only marked 421s | `a_guest_cannot_pose…` |
| M10 | stripping the marker from guest answers | `a_guest_cannot_pose…` |
| M11 | the caller chain in the cycle check | `calls_reach…` (waits out the timeout) |
| M12 | pushing renewals to workers | `calls_reach…` (y's epoch rises) |
| M13 | evicting idle objects on a lapse | `a_node_that_lost…` |
| M14 | releasing on actor exit | `a_guest_cannot_pose…` (not released) |
| M15 | a new session after a lapse | `a_renewing_holder…` |
| M16 | the node in the rendezvous score | `rendezvous_moves_only_what_it_must` |

Two mutations survived and led to removing code instead of keeping it
untested. An early check in `object::commit` was redundant with the
journal's (now the journal checks commits of nothing too). A per-request check
in the actor's loop was unreachable but for a window of microseconds: a request
that runs across a loss fails its commit and resets the instance, and the next
one goes back through the lease request.

Full suite on the final tree (`--lib --test integration --example
committed_state`): 114 unit tests pass; integration 74 pass, 13 ignored
(opt-in), 1 fails, `kv_counts_against_the_disk_quota`, with `"error":"cpu"`:
the known load failure N-640 (CPU budget billed as wall time; fixed on
`cf/cpu-clock`), here at load 9–15. `committed_state` 2 pass. The production
check (`cargo check --bin nanovm`) is clean.

## Measurements

Single node, A/B on the HTTP path, method from the shared bench harness (CPU
per request from `/proc` for every process involved, alternating rounds,
100,000 requests at 64 concurrent to 100 counter objects, after a warm-up).
The base is `cf/main-next` 3dafe33, built exactly like the branch (the
worktree's Cargo config, dev profile: dependencies at opt-level 3, nanovm at
1; no release builds, to save disk). The machine was shared (load 14–30), so
spreads are wide; medians:

| path | base µs/req | branch µs/req | rounds |
|---|---|---|---|
| node (supervisor + worker) | 105.6 (91.7–125.7) | 102.0 (88.7–118.4) | 7 each |
| supervisor alone | 30.5 | 30.7 | |
| worker alone | 75.1 | 71.0 | |
| proxy → node (all three) | 181.1 (179.0–188.9) | 180.0 (135.9–185.4) | 3 each |
| proxy alone | 45.2 | 42.7 | |

No measurable cost: with one node there is no store, no session and no lease
traffic. The per-request additions are an `Option` check per request, a
header lookup and a header removal. A first A/B against a base built outside
`/home/user/wt` showed the branch 19% slower. That base had escaped the
worktree Cargo config (`debug = 0`, `strip = "debuginfo"`), so it was rebuilt
alike and the result discarded.

Runtime lines (CI's count): 5,184 at the old base, 5,653 at the branch before
the merge (+469; `lease.rs` 223, proxy 65, table 65, supervisor 51, object
27, egress 19, others 19). See the final report for the count after the
merge.

## Not done

- Volumes don't move between nodes: B starts from its own disk unless the
  nodes share a data root (the failover test's stand-in). Needs W-9's stream
  and W-90.
- The epoch isn't yet in any committed version (W-90).
- A cycle of calls closed through waits on two other nodes is ended by the
  timeout, not refused (N-180).
- Cold starts with leases wait on the store (N-183; W-91 proposes keeping
  released leases for a while). Renewals have object_store's own timeouts
  (30 s), so one slower than the lease just lets the session lapse (safe).

## Decisions for the owner (numbered, with a recommendation)

1. **Lease duration vs failover time.** An object whose node stops is
   unavailable for about `lease_ms` after the first request that finds it, and
   the proxy holds such requests up to `failover_ms`. Shorter leases mean
   faster failover but more renewals (one conditional PUT per `lease_ms / 4`
   per node: 0.4/s at 10 s, about $0.17 a day per node on S3 Standard), and a
   store latency spike of `lease_ms` / 8 can cost a node its session.
   *Recommend 10 s, `failover_ms` 30 s.*
2. **The store as a hard dependency (N-181).** If it is unreachable for
   `lease_ms`, every node stops serving. *Recommend accepting it, as with any
   lease service, and choosing a store at least as available as the cluster
   must be (S3, R2).*
3. **No clock sync.** Expiry is judged by each node's own monotonic clock
   (Kubernetes' and DynamoDB lock client's approach), so only clock rates must
   agree within the 1/8 margin. A wall-clock expiry would allow taking over
   at once a lease that ran out long ago, but needs NTP within a margin.
   *Recommend keeping observation.*
4. **`object_store` as a dependency** (0.14: aws-base, reqwest, ring; 7 new
   crates). It gives S3, R2, MinIO and an in-memory fake. W-9's stream will
   need the same client. A hand-rolled SigV4 client would be about 150 more
   runtime lines. *Recommend keeping it.*
5. **Calls go through the proxy** with leases, even to an object on the same
   node: two more hops, but nodes stay unaware of each other (Q49).
   *Recommend keeping it until calls between objects show up in profiles.*
6. **Cross-node cycles (N-180):** refusing all of them needs a wait graph
   shared between nodes. *Recommend the timeout until a real app needs more.*
7. **The proxy buffers bodies with more than one node (N-182).** *Recommend
   bounding it with W-35's proxy-wide budget.*
