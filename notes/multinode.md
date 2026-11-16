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
the request, so retrying is safe without request ids. A node that took the
request and hasn't answered by the deadline may have run it: the proxy answers
504 and never sends it again (`review_failover_deadline_bounds_a_connected_silent_node`). With more than one node
the proxy buffers request bodies (up to `max_request_body_kb`) so it can send
them again; with one node it streams as before and never retries.
WebSocket events and calls between objects take the same path; a call's object
is its URI's path, the query going on with it
(`review_cross_node_root_calls_accept_a_query_without_a_trailing_slash`).

**Leases (`lease.rs`).** With `leases = "s3://bucket/prefix"` on the nodes,
the leases live in an object store, through the `object_store` crate (S3 and
S3-compatibles such as R2 and MinIO; credentials from `AWS_*` variables). Its
in-memory store is the test fake. Every write is conditional (`If-None-Match:
*` to create, `If-Match` to update), so of two nodes racing one wins.
- `objects/<app>/<object>` is `{node, session, epoch}`. Taking it writes
  `epoch + 1`, conditionally on what was read, so epochs only ever rise,
  across all nodes. Releasing writes no node, same epoch.
- `nodes/<node>` is `{session, beat, lease_ms}`: one record per node renews
  all its leases (one conditional PUT every `lease_ms / 4`, 0.4/s at the
  default 10 s), not one per object. It carries the node's own `lease_ms`,
  which another node waits out before taking the node's objects, so nodes
  needn't agree on it (`review_shorter_observer_ttl_cannot_revoke_a_live_holder`:
  a taker configured with 100 ms took a 1 s holder's object 766 ms early).
- A node's leases hold until 7/8 of `lease_ms` after it **sent** its last
  renewal that succeeded. After that its workers serve none of those objects
  and commit nothing; the supervisor then starts a new session (`session + 1`).
- Another node takes an object whose holder's session record it has seen
  unchanged for the holder's whole `lease_ms`, by its own monotonic clock. So clocks need
  not agree, only tick at about the same rate (the 1/8 margin). Before taking
  the object it **ends** that session with a conditional write
  (`session + 1`), so that a renewal already on its way can't land afterwards
  and extend the holder's leases. Without that write a takeover and a late
  renewal could both succeed (`a_late_renewal_cannot_outlive_a_takeover`).
- Without a store (`leases = ""`) there is one node and every lease is its
  own at epoch 0: no store, no session, no extra work per request.

Why it is safe: B's takeover write at time k succeeded only if A's session
record was still at the version B had seen for A's `lease_ms`. So A's last
successful renewal was sent at least that long before k, and A's leases ended
an eighth of it before k. Every later renewal by A fails its condition.

A grant is in the session its acquisition began in, so it counts only while
that session is still the node's and live once the store has answered: the
supervisor checks that, and answers the worker with that session's deadline,
never a later session's (`Leases::until`). A worker's session only moves
forward (a later session, or a later deadline for this one), since the
supervisor's words can arrive out of order, and a worker refuses a grant that
doesn't hold in the session it knows. Before, a store slow to answer let the
session lapse, another node take the object, and the node start a new
session; the worker then got the dead session's id with the new deadline and
committed at epoch 1 beside the new owner's 2
(`review_delayed_acquire_does_not_revive_a_dead_session`).

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

The local check is not the fence: it stops a node that knows it lost the
lease, but one that doesn't know yet (a long pause between the check and the
new key) can still commit locally. That is harmless only because a version
can't count off the node without a conditional write that a takeover has
already defeated: the head, below.

### Committed state across nodes: the head (W-90, `fix/lease-r3fix`)

An object's committed versions (W-9's capture, each carrying the
lease epoch) are shipped by the supervisor to `capture.store`, which every
node shares. With leases a node must have one: another node restores an
object it takes over from there.

**Keys.** A version is put once at `<app>/<object>/<sequence>-<sha256>.tar`.
The key names the bytes, so writers never clash: a version a node uploads
after losing the object lands under its own key and is simply never named.
A directory store puts with a hard link from a fresh, exclusively created,
synced temporary file, then syncs the directory (and the directories it
made).

**The head.** `<app>/<object>/head` is `{sequence, digest, node, lease}`: the
last version that counts, and the node that last claimed the object with
the epoch of its lease then. A version counts only once the head names it.
Every write of the head is conditional on the version read (`If-Match` the
ETag on S3 and compatibles; on a directory store, a rename under a lock file,
compared by digest):

- **Claim** (`ship::claim`, in the supervisor, before a lease's grant goes to
  the worker): the head's `node` and `lease` become this node's at the new
  epoch. From then on no node with an earlier lease can move it, and its
  in-flight move fails its condition. A head claimed at a later epoch refuses
  the claim (the grant is stale), as does a store error: the grant is
  released and the worker answers 421.
- **Ship** (`ship::ship_object`): only under this node's live lease on the
  object (a grant its worker holds, in the node's current session, not
  lapsed; `Leases::epoch`), only if the head is claimed by this node at that
  epoch, and only a version that follows the head (the next sequence, whose
  parent digest is the head's). Upload, then move the head; then mark it
  shipped locally. A move that fails its condition stops shipping: the node
  lost the object. Up to 32 objects ship at once, each its versions in
  order; the head is read once and then moved from what this node last
  wrote (a stale one fails the move and is read again), so a version costs
  a put and a head move. A failure is its object's own (logged, tried
  again a second later); holding no lease is a state, not a failure.
  A version up to the head's is marked, not refused, when this copy has
  the head's version: a marker is unsynced, and one a power loss takes
  back is caught up from the head.
- **Restore** (`ship::restore`) follows the head back, parent by parent, to
  the last snapshot: only versions from there are fetched (N-166's
  pointer).
- **Acknowledgement** (`remote_ack`) waits for the local marker, which is
  written only after the head names the version. An instance then starts
  only once every version here counts (`ship::caught_up`): a commit whose
  wait ran out is answered 500 but stands here, and serving it would show
  state a takeover can lose. The object answers 503 `not-stored`
  (`Retry-After: 1`) meanwhile, holding its lease so its shipper can ship.

**Takeover restores before serving.** The claim decides whether this node's
copy of the object is the head's: it must hold the head's version, and
anything after it counts only if no other node has claimed the object since
this node did (the head's `node` is this node), or, if another has, the copy
must be exactly the head's (no later or staged versions), and a copy with
versions must have its volume. Otherwise the copy goes (the volume first,
then the journal and the marker, the versions last, so a discard cut short
leaves a copy judged as before, or an empty one: never a current one), the
drop is synced, the head is claimed, and the object is rebuilt from the
head; the supervisor gives what it restored to the app's uid. The claim
lists this copy before it reads the head: the shipper, which may still be
shipping under the grant before this one, removes a version only after
moving the head past it, so the head read after names a listed version if
this copy has it (`a_claim_racing_the_shippers_old_grant_keeps_later_versions`). Only then does the worker instantiate. So a
node never serves state older than the head, and history a node committed
but never shipped before another took the object is dropped there, never
shipped.

**Acknowledgement with leases is remote.** A node with leases answers a write
only once its version counts in the store (the owner's decision): failover
then loses nothing that was answered. `capture.remote_ack` defaults to true
when `leases` is set (`false` keeps local acknowledgement, which a failover
may lose); one node keeps local by default.

**Why it is safe.** A version counts only through a conditional move of the
head from the state its writer read. A takeover's claim moves the head
before the new owner serves, so the old owner's move (from a state before
the claim) fails, however late its upload lands. The new owner's copy is
the head's before it serves, and its versions follow the head. Each claim
raises the head's lease epoch, so the old owner can't claim it back with its
stale grant either.

The head lives in the capture store, not in the lease record as first
proposed: the capture store is already shared and written conditionally, the
head carries the claiming epoch itself (so it doesn't depend on the lease
record's version, which every acquisition and release changes), and one node
uses the same head at epoch 0. It costs a claim (a read and a conditional
write) per acquisition, and per shipped version a put and a conditional
write (the head is read again only after a claim or a failed move); S3 needs its default `ETagMatch` conditional puts, as the
leases do.

**Never delete lease records or heads**: an object's epochs would restart,
and a head's history with them.

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

From the round-3 review (`fix/lease-r3fix`; each failed before its fix, with
the evidence in its commit, and is mutation-checked below):
- `review_delayed_acquire_does_not_revive_a_dead_session` (M1),
  `review_shorter_observer_ttl_cannot_revoke_a_live_holder` (M4): above.
- `review_takeover_during_upload_fences_the_old_writer` (M2): A's upload
  waits; B takes the object over; the upload lands. A's shipping fails and
  marks nothing; B's version 1 is the head's, and a fresh root restores B's
  (before: A's counted, B's conflicted).
- `review_shipping_requires_a_live_matching_epoch_not_just_a_node_name` (M3):
  no lease, a released one, a lapsed one, and one taken again after B held
  the object: none ships (before: all four did).
- `a_claim_keeps_only_history_no_other_node_has_moved_past`: a node keeps
  its copy (and ships its later version) when it takes the object back with
  no other node between; after another node held it, its unshipped version
  goes and the head's is restored.
- `review_concurrent_directory_put_keeps_acknowledged_bytes` (S3): racing
  directory puts at one key: one stores, the others are refused, the bytes
  stay.
- `review_failover_deadline_bounds_a_connected_silent_node` (M6, proxy).

Integration (`tests/integration.rs`, `mod nodes`; real jailed workers, two
in-process supervisors, a proxy, a shared in-memory lease store, and a shared
directory capture store, acknowledging remotely):
- `review_failover_with_separate_disks_preserves_remote_acknowledged_state`
  (M5): nodes on data roots of their own. A's write is answered with the head
  already naming it; A stops; B serves `one` at epoch 2, restored from the
  store (before: B served an empty object), and its next write moves the head
  to 2.
- `review_cross_node_root_calls_accept_a_query_without_a_trailing_slash` (M7):
  `http://nanovm/objects/{y}?v=1` from x on A reaches y on B (before: 400).
- `an_object_moves_to_another_node_when_its_node_stops`: A serves and commits
  `one`. A stops: its own runtime is dropped, killing its server, renewals and
  workers. B serves `one` (here the nodes share a data root: B keeps the copy,
  which is exactly the head's) at epoch 2, no sooner than `lease_ms` later.
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
committed_state`, since removed): 114 unit tests pass; integration 74 pass, 13 ignored
(opt-in), 1 fails, `kv_counts_against_the_disk_quota`, with `"error":"cpu"`:
the known load failure N-640 (CPU budget billed as wall time; fixed on
`cf/cpu-clock`), here at load 9–15. `committed_state` (since removed) 2 pass. The production
check (`cargo check --bin nanovm`) is clean.

Mutation checks for the review's fixes (`fix/lease-r3fix`; each removes one
mechanism, the named test then fails; restored after):

| # | removed | caught by |
|---|---|---|
| R1 | a head move conditional on the head read (S3 store overwrites) | `review_takeover_during_upload_fences_the_old_writer` |
| R2 | the lease's liveness in the shipper's authority | `review_shipping_requires_a_live_matching_epoch_not_just_a_node_name` |
| R3 | the shipper's check that the head is claimed by this node at this epoch | same |
| R4 | the claim's currency check (any copy kept) | `a_claim_keeps_only_history_no_other_node_has_moved_past` |
| R5 | dropping versions written before another node's claim | same |
| R6 | the worker's forward-only session | `review_delayed_acquire_does_not_revive_a_dead_session` |
| R7 | `serve` answering with the grant's own session's deadline | same (it survived until the proof also covered a worker that hadn't heard of the new session) |
| R8 | the holder's `lease_ms` (the taker's own instead) | `review_shorter_observer_ttl_cannot_revoke_a_live_holder` |
| R9 | a fresh exclusive temporary file per directory put | `review_concurrent_directory_put_keeps_acknowledged_bytes` |
| R10 | the proxy's deadline on a connected node's answer | `review_failover_deadline_bounds_a_connected_silent_node` |

Suite on `fix/lease-r3fix`: `--lib` 151 pass; integration 92 pass, 14
ignored (opt-in); the SQLite suite (`sqlite_ -- --ignored`) 12 pass; `dev`
5 pass, 4 need the example and JavaScript app builds (absent here);
`committed_state` (since removed) 2 pass; `cargo check --bin nanovm` clean. Runtime lines
7,068 (cf/round3 9f878a9) -> 7,301 (+233: ship.rs +175, lease.rs +25,
supervisor.rs +20, config.rs +8, proxy.rs +5).

### Second lease review (`fix/r3-lease`, N-720..N-729)

`review2/lease` added four failing tests; each passes now, its root cause
fixed and the test kept. The review's two suspicions were proven by tests
that failed, then fixed; the deployable agent's B-604 too. Each test below
failed before its fix (the runs are in the commits) and is mutation-checked.

- **N-720** `mark` wrote every marker through one fixed temporary file; now
  a file of its own (`written`), unsynced (a marker behind is caught up,
  N-729). `review_concurrent_claims_mark_each_object_its_own_head`: 66 and
  41 of 320 claims wrong before.
- **N-721** a copy with versions is current only with its volume, which a
  discard removes first; the marker goes before the versions, which go last.
  `review_a_discard_cut_short_is_not_taken_for_a_current_copy`.
- **N-722** a claim syncs what it dropped before its head write.
  `a_claim_syncs_what_it_dropped_before_claiming` (a failing sync must stop
  the claim; a test-only hook in `sync_fs`).
- **N-723** (suspicion) the claim lists this copy before reading the head.
  `a_claim_racing_the_shippers_old_grant_keeps_later_versions`: a store
  holds the shipper's put and the claim's head read; before, version 5
  (committed after the shipper listed its own) was dropped.
- **N-724** up to 32 objects ship at once, each in order; a head is read
  once, then moved from what this node wrote; failures are per object.
  `review_remote_acks_keep_up_with_objects_committing_at_once` (14 of 40
  late before), `the_shipper_reads_a_head_once_then_moves_it_from_what_it_wrote`.
- **N-725** with `remote_ack`, an instance starts only once every version
  here counts (`ship::caught_up`), else 503 `not-stored`, `Retry-After: 1`.
  `review_a_takeover_never_serves_older_state_than_was_served` (its "next
  instance" now starts as the node starts one) and
  `nodes::a_write_answered_500_is_served_only_once_it_counts` (the write
  is answered 500, or 504 when the request's deadline ends the wait first).
- **N-726** (suspicion) a claim that finds the history broken in the store
  (`NotFound`, `InvalidData`) refuses the object on that node for 60 s
  without asking the store.
  `an_object_whose_history_is_broken_is_refused_without_asking_the_store_again`
  (before: 5 requests, 5 claims, epoch 5).
- **N-727** `lease_ms = 0`, non-shared lease or capture stores in a config
  file with leases, and `failover_ms` at or below the proxy's connect
  timeout are refused. `configs_that_cannot_hold_leases_are_refused`.
- **N-728** (B-604) no lease is `Ok(false)`, never an error; every error is
  logged and its object retried a second later.
  `a_failure_shipping_one_object_is_its_own_and_not_taken_for_no_lease`
  (before: 24 quiet tries in 500 ms).
- **N-729** a version up to the head's is marked when this copy has the
  head's version. `the_shipper_catches_up_a_marker_left_behind`.
- **B-502** (recovery agent's) the supervisor, as root, marks, drops and
  prunes in the app's directory, which the app's worker writes, by name from
  descriptors opened with `O_NOFOLLOW` (`committed::dir_at`, `remove_at`):
  a planted symlink is refused, never followed.
  `the_supervisor_never_follows_a_symlink_the_app_planted` (before: the
  discard deleted outside through `~journal`).
- **B-507** (recovery agent's) the wait for the store's acknowledgement is
  bounded by its own `ship::ACK` (2 s), never by the app's deadlines: a
  commit runs in a `committing` phase whose deadline is the app's
  post-return deadline plus `ACK`, and the guest's post-return clock starts
  after it. `a_slow_acknowledgement_is_not_cut_short_by_the_apps_deadline`
  (before: 504 "deadline exceeded while post-return").

Mutation checks (`fix/r3-lease`; each removes one mechanism, the named test
then fails; restored with `git checkout`, empty diff after):

| # | removed | caught by |
|---|---|---|
| L1 | a marker file of its own (the fixed `~new` again) | `review_concurrent_claims_mark_each_object_its_own_head` (the runs before the fix) |
| L2 | a current copy has its volume (`whole` always true) | `review_a_discard_cut_short_is_not_taken_for_a_current_copy` |
| L3 | the sync after a discard | `a_claim_syncs_what_it_dropped_before_claiming` |
| L4 | listing the copy before reading the head | `a_claim_racing_the_shippers_old_grant_keeps_later_versions` |
| L5 | objects shipped at once (32 -> 1) | `review_remote_acks_keep_up_with_objects_committing_at_once` |
| L6 | the head kept from the last move | `the_shipper_reads_a_head_once_then_moves_it_from_what_it_wrote` |
| L7 | errors as errors (PermissionDenied taken for no lease again) | `a_failure_shipping_one_object_is_its_own_and_not_taken_for_no_lease` |
| L8 | marking versions up to the head | `the_shipper_catches_up_a_marker_left_behind` |
| L9 | `caught_up` (always caught up) | `review_a_takeover_never_serves_older_state_than_was_served` |
| L10 | the instance start's call to `caught_up` | `nodes::a_write_answered_500_is_served_only_once_it_counts` |
| L11 | the remembered broken history | `an_object_whose_history_is_broken_is_refused_without_asking_the_store_again` |
| L12 | refusing `lease_ms = 0`; refusing stores nodes can't share; refusing `failover_ms` at the connect timeout (three mutations) | `configs_that_cannot_hold_leases_are_refused`, each time |
| L13 | descriptors for `mark`, `discard` and the prune (paths again) | `the_supervisor_never_follows_a_symlink_the_app_planted` |
| L14 | the `committing` phase (the commit under the post-return deadline again) | `a_slow_acknowledgement_is_not_cut_short_by_the_apps_deadline` (504) |

L2–L9, L11 and L12 ran as three builds of independent mutations; in each
only the named tests failed (`ship::`, `lease::`, `config::`, `proxy::`:
37 tests). L10 also fails `an_unreachable_store_withholds_acknowledgement_and_bounds_the_outbox`.

**Shipper throughput** (`ship::tests::measure_remote_acks`, ignored; N
objects each commit a version and wait for it, as a `remote_ack` commit
does, again and again for 10 s; the store answers every call in 25 ms;
CPU is the shipper runtime's threads, from `/proc/self/task/*/stat`). The
baseline is the reviewer's tree (0ea78cd) with the same test; the two test
binaries alternated, 3 rounds each, on the shared VM (load 21–25).
Medians [ranges]:

| objects | baseline acks/s | late | CPU µs/ack | fix acks/s | late | CPU µs/ack |
|---|---|---|---|---|---|---|
| 1 | 9.1 [7.6–9.7] | 0 | 439 [259–510] | 14.6 [12.9–15.4] | 0 | 620 [544–645] |
| 40 | 5.9 [5.9–6.1] | 165 [163–169] | 1,200 [845–1,408] | 548 [512–565] | 0 | 244 [194–261] |
| 200 | 5.4 [5.3–5.7] | 978 [968–980] | 923 [857–1,363] | 559 [393–583] | 0 [0–221] | 295 [275–301] |

"Late": commits not acknowledged within the 2 s wait in 10 s (answered 500
though they stand). The baseline costs three round trips a version, one
object at a time; the fix two, 32 objects at once (a ceiling of 640/s at
25 ms), and one object's acknowledgement takes the scan's 20 ms plus two
round trips. CPU per acknowledgement fell 4–5x with many objects; with one
it rose (the per-scan cost, now a task per changed object, spread over fewer
acknowledgements: about 9 ms of CPU a second in all). The one 200-object
round with late commits ran at load 24, with the test's 200 threads each
polling its marker every 2 ms, as a `remote_ack` commit does (N-165).


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

- Remote retention: every version stays in the store; a restore fetches only
  from the last snapshot before the head (N-166).
- A takeover restores the whole object (from its last snapshot) before
  serving it, on the cold start's path: a large volume delays its first
  request after failover. The claim adds a head read and a conditional write
  to each acquisition (N-183).
- One node streams to its node without a deadline (no failover to bound it).
- A cycle of calls closed through waits on two other nodes is ended by the
  timeout, not refused (N-180).
- Cold starts with leases wait on the store (N-183; W-91 proposes keeping
  released leases for a while). Renewals have object_store's own timeouts
  (30 s), so one slower than the lease just lets the session lapse (safe).
- From the second review (`fix/r3-lease`): an object refused for a broken
  history answers the worker 503, which the worker turns into 421, so the
  client gets the proxy's 503 after `failover_ms`, not the reason (N-726).
  An instance waiting for its versions to count holds a pool slot for up to
  2 s. The shipper holds up to 32 versions in memory at once (a snapshot is
  the whole volume). It still finds new versions by polling (N-165), and a
  `remote_ack` commit polls its marker every 2 ms. The permissions behind
  B-604 (a supervisor without `CAP_DAC_OVERRIDE` writing in app-owned
  directories) are the deployable agent's; failures now show in the log.
  The shipper still reads versions by path (a planted symlink can make it
  read another directory's); beta/recovery's identity check (a version must
  name its own app and object) refuses what it reads that way. With the
  capture store down, a node can't claim an object (as with the lease
  store, N-181): an object whose actor has retired is refused (421) until
  the store is back, and the proxy, retrying until `failover_ms`, answers
  504 if its deadline lands during a try, 503 otherwise.

## Decisions for the owner (numbered, with a recommendation)

1. **Lease duration vs failover time.** An object whose node stops is
   unavailable for about `lease_ms` after the first request that finds it, and
   the proxy holds such requests up to `failover_ms`. Shorter leases mean
   faster failover but more renewals (one conditional PUT per `lease_ms / 4`
   per node: 0.4/s at 10 s, about $0.17 a day per node on S3 Standard), and a
   store latency spike of `lease_ms` / 8 can cost a node its session.
   *Decided: 10 s, `failover_ms` 30 s.* Nodes may now differ: each waits out
   the holder's own duration. `failover_ms` should exceed the longest.
   A request a node took but hasn't answered within `failover_ms` gets a 504,
   so it also bounds the longest request through the proxy with several nodes
   (queueing included).
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
8. **Acknowledgement with leases.** *Decided: remote.* A config file with
   `leases` defaults `capture.remote_ack` to true (a write is answered once
   the head names its version: about four store round trips, tens of ms on
   S3); an explicit `false` keeps local acknowledgement, which a failover may
   lose (the claim drops versions the head never named). Programmatic configs
   (tests, embedding) set it themselves. One node stays local by default.
   Since `fix/r3-lease` a write is answered once the head names its version
   (a put and a head move, two round trips, plus up to 20 ms of polling), and
   an object with a version that doesn't count yet serves nothing (503
   `not-stored`) until it does: with the store down, reads of an object whose
   last write failed fail too, as remote acknowledgement means.
9. **A head written without leases (`lease` 0).** A claim treats such a head
   as this node's own (`current`: `head.lease == 0`), so it keeps any versions
   after it. That matters only when a cluster with leases takes over a store a
   single node wrote, and then only for whichever node claims first: if that
   is the former single node, its unshipped versions survive (and ship); if
   another node claims first, they are dropped. *Recommend treating a lease-0
   head like any other node's* (remove `|| head.lease == 0`, one line): the
   switch to leases then never keeps unshipped single-node history, whichever
   node claims first, and the operator drains the single node (lets its
   shipper catch up) before turning leases on. Refusing such heads with an
   error instead (about three lines) would cost every single-node deployment
   its store when it grows to several nodes (a fresh store, history lost),
   for no safety the one-line change doesn't give.
