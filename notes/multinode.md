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
- refuses a request with 421 when the object's lease is lost, both when
  queueing it and again when it reaches the front of the queue (so it never
  runs on a stale instance);
- evicts idle objects at once when the session lapses or changes; busy ones
  stop at their next request or commit;
- refuses any commit once the lease is gone (`Node::lease_epoch`, below);
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

## The hook for W-9 (storage agent, `cf/storage`)

```rust
// object.rs, commit(): the one line on the commit path
let _epoch = node.lease_epoch(&shared)?;   // Err once this node lost the lease
quota.commit()?;
```

`Node::lease_epoch(&ObjShared) -> io::Result<u64>`: the epoch of this node's
lease on the object, or an error once the node no longer holds it. It is 0
with one node. The contract for joining it to the commit stream:

1. **Identity.** A committed version is named `(app, object, epoch, seq)`.
   Epochs rise strictly per object across nodes. `seq` restarts or continues
   per epoch, as the stream chooses, as long as replay orders by
   `(epoch, seq)`.
2. **The local check is not the fence.** It stops a node that knows it lost
   the lease, cheaply, before the local journal commit. A commit that is slow
   (fsyncs) can still end after the lease lapses. The fence that makes a lost
   node's commit harmless must be at the store: **the write that makes a
   version durable off the node must be conditional on the lease record not
   having moved**. Proposed: keep the stream's head in the lease record
   (`objects/<app>/<object>` gains `head`), written `If-Match` the version the
   grant (or the previous commit) produced. A 412 means another node took the
   object: report the commit as failed and reset the instance. `Grant` already
   carries that version (`e_tag`, `version`); the supervisor, which holds the
   store client, would do the write when the worker ships the version. This
   costs one store round trip per commit, which W-9's remote durability needs
   anyway.
3. **Takeover.** B gets epoch e+1 only after ending A's session. Before
   serving, B must restore the object up to the head recorded in the lease
   record it overwrote. Until the stream exists, the failover test shares one
   data root between the nodes, standing in for it.
4. **Never delete lease records**: an object's epochs would restart.

## Tests (all hostile; each mutation-checked)

Unit (`lease.rs`, in-memory store):
- `leases_pass_from_node_to_node_at_rising_epochs`: grant at epoch 1, refusal
  naming the holder, release then take at 2, a stale release changes nothing,
  a takeover only after a whole lease unchanged (3), re-take (4).
- `a_renewing_holder_keeps_its_leases_and_a_lapsed_one_loses_them_first`: a
  renewing holder is never taken over (3 leases long). One gone quiet has
  lapsed before the takeover, which comes at least `lease_ms` after its last
  renewal.
- `a_late_renewal_cannot_outlive_a_takeover`: a renewal sent before a takeover
  and landing after it fails.

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

MUTATIONS

## Measurements

MEASUREMENTS

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
