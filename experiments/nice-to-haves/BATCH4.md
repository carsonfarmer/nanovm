# Batch 4 — application protocol teaching models

`protocol_models.py` implements NH21/NH22 as deterministic, bounded in-memory
examples. It has no sockets, nanovm imports, disk state, worker changes or alarms.
Run `python3 -B experiments/nice-to-haves/protocol_models.py` for a fixed JSON
transcript: a lost response retries to sequence 1, a changed body conflicts,
duplicate delivery displays only once, and the same ID **executes again** at TTL.

The room instance supplies the tenant/object scope; authenticated caller identity
must come from a trusted application layer, never a client header. IDs are scoped
by caller and compared against exact UTF-8 bodies (no Unicode normalization).
Accepted records expire at `accepted_time + TTL`, including the exact boundary;
retries do not extend retention. Full tables reject new operations without evicting
live records. A separate bounded history ring supports reconnect cursors; eviction
returns `gap`, not silent history loss. The example client advances only for the
next contiguous sequence. It ignores old sequences only within one room session.
It does not implement a UI, pending outbox, snapshot installation or persistence.

**This is not an exactly-once service.** Expired IDs and new model instances can
accept an old request again. Real applications need an explicit stale-operation
policy, authenticated session/room identity, an atomic mutation-plus-receipt
boundary, concurrency control and durable retention. None is supplied here.
Do not reuse sequence cursors across rooms or server incarnations. Synthetic
clock and single-threaded execution are assumptions, not production guarantees.

Six contract tests cover lost replies, caller/room scope, conflicting bodies,
Unicode-byte differences, TTL boundaries, no sliding expiry, full capacity, ring
gaps, retained receipts after ring eviction, copied replay data, contiguous client
cursors and byte limits. The fixture checks outcomes, not private call order.

## NH29: conceptual scheduler HTTP contract (proposal only)

This is a downstream application API sketch, **not an existing nanovm API**:

```http
PUT /app-schedules/daily-report
If-Match: "revision-3"
Content-Type: application/json

{"due_at":"2027-01-01T09:00:00Z","payload":{"report":"daily"}}
```

An authenticated application service would validate ownership, reject mismatched
revisions with 412, return a new revision and arrange an external delivery.
Delivery carries `(schedule_id, revision, occurrence_id)` plus a signed audience
and expiry. A duplicate occurrence returns the previously recorded application
result; the same ID with changed payload conflicts. Cancellation uses the expected
revision and prevents **future dispatch**, not effects already accepted by a
recipient. An in-flight old revision may still arrive and must be rejected by the
recipient's application policy. Recreating a schedule needs a new generation so
old deliveries cannot match. Product decisions remain: missed-run catchup, timezone
and DST, retry horizon, cancellation races, quotas, auth, billing and retention.
No live scheduler, timing guarantee or production alarms are implemented.

## NH35: tenant-safe incident worksheet (recipe)

1. Record UTC interval, build/component digest, request layer and affected count.
2. Replace tenant/object/user IDs with investigation-local aliases; store the
   mapping separately with restricted access. Do not copy bodies, cookies, bearer
   tokens, query strings or raw environment values into an incident document.
3. Separate observed facts (raw evidence location and access owner), hypotheses,
   contradictory controls and unknowns. A timeout is an unknown mutation outcome.
4. State user-visible symptoms, containment authorization, and reversible actions.
5. Record exact test inputs and status oracle; compare unaffected tenants using
   aggregate counts. Equal export counts do not prove unique span delivery.
6. Before sharing, have a second person check free text, screenshots and attachments;
   a regex scrubber is not a privacy guarantee. Set retention/deletion ownership.

## NH38: trust-boundary exercise and answer key (recipe)

Scenario: a browser sends `X-Tenant: admin`, `X-Region: trusted`, an operation ID,
and a trace parent. A downstream service copies them into authorization decisions
and a deduplication key. A proxy is present, so the developer assumes they are safe.

Questions: Which values prove identity? Can a second user suppress an operation
using its ID? Does a signed component authenticate a browser? Does tracing confer
authority? Does a response timeout mean retrying under a new ID is safe?

Answer key: none of these browser values proves identity. Authenticate and authorize
at the application trust boundary; strip/replace reserved headers there and prevent
bypass. Bind receipts to authenticated tenant/object/principal scope and exact
payload. A component signature identifies an artifact, not an end user. Trace
context is correlation input, not authorization. Operator enrichment must not
overwrite supervisor/app identity. A timeout can follow a successful mutation;
use the application's retry contract and original ID, never infer rejection.
This is a teaching scenario, not a claim that the current host has these defects.
