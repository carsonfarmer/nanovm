# Application and workload recipes

These are **design recipes**, not implemented guests or tested application services.
They require no host API change. Current behavior is described in the repository
[README](../../README.md); the [toolbox](README.md) tests only its own client/tools.

## Operation IDs belong to the application

The [HTTP example](http_operation.py) sends an opaque `Idempotency-Key`. nanovm
does not interpret it. An application that wants retry-safe mutations needs this
contract before a client can treat resending as safe:

1. Authenticate the caller at the application's chosen boundary. Scope an ID by
   tenant, object, caller and operation kind, not globally by a client string.
2. Validate the body before mutation. Define a fingerprint over method, logical
   operation, API version and exact body bytes (or documented canonical JSON).
   Do not fingerprint arbitrary JSON serialization differently across languages.
3. Look up the scoped ID. Same fingerprint: return the saved application response;
   different fingerprint: return a conflict without applying the operation.
4. For a new ID, update domain state and the saved fingerprint/response in the
   same application request unit. Do not send success and then save the ID in
   post-return work. A caught application error or HTTP 500 is not automatically
   a rollback request; validate before writing or explicitly manage app semantics.
5. Define retention by business need and quotas. Store bounded responses and bound
   ID count/bytes. Expired IDs cannot promise deduplication; reject old requests or
   require clients to use the application's documented retention window.
6. Return the ID with the result. A status lookup endpoint is application-defined,
   authorization-scoped and must distinguish unknown, expired and completed.

This does not provide exactly-once external side effects. An external payment or
email may happen even if the application request fails; the remote service needs
its own idempotency protocol and reconciliation. No new host guarantee is implied.

Acceptance matrix for a future application example:

| Input sequence | Required observable result |
| --- | --- |
| New ID, body A; repeat same ID/body | One domain mutation, same saved result |
| Same ID, body B | Conflict, state unchanged |
| Two callers reuse same textual ID | Authorized caller-scoped behavior, no response leakage |
| Client disconnects after sending | Unknown client outcome; same-ID reconciliation, not new-ID retry |
| ID expires | Explicit expired/retention policy, no unlimited guarantee |
| Response or ID budget exhausted | Defined refusal before mutation; never evict live keys silently |

No persistence, failure-recovery, or guest correctness claim is made by this recipe.

## Workload cookbook with explicit oracles

Keep component digest, request distribution, body bytes and concurrency in every
run's manifest. All mutation workloads require a disposable application and owner
authorization. These recipes do not start traffic themselves.

| Workload | Input | Check | Avoid claiming |
| --- | --- | --- | --- |
| One hot room | One object, concurrency 1 then 16 then 64 | Successful operations, queue rejection counts, response latency | Extra clients make a serialized object parallel |
| Many warm rooms | Fixed 100 objects, warmup separate from timed interval | Equal per-object request counts, errors, selected-layer spans | In-process rate equals public HTTP rate |
| Cold object tour | Fixed finite list of new IDs on a disposable deployment | Cold counts, instantiation errors, peak RSS | Fresh IDs are free or persistent data disappears on eviction |
| Read-mostly ledger | 95 reads / 5 writes with seeded order | Application checksum plus read/write totals | Catching failures demonstrates correct state |
| Payload boundary | 0 B, 128 B, 4 KiB, declared app max, max+1 | Exact response bytes or declared refusal | A logical payload size is physical storage consumption |
| Dependency fanout | 0, 1, 4 application calls with controlled responses | Parent/child traces, handled dependency failures | Parent HTTP200 means every dependency succeeded |
| Version presentation | Same app/object, separately approved component versions | Response API compatibility and client decoding | This tests deployment draining or runtime correctness |
| Telemetry comparison | Same workload with separately budgeted Collector | Expected layer counts after drain and application CPU | Successful HTTP proves complete export |

Prefer a short bounded run before a long one. Record every run, including failures;
do not drop slow samples or silently change concurrency to get a preferred result.

## Chat client protocol sketch

This sketch is a proposal, not a wire-compatible replacement for the proxy's
WebSocket-over-HTTP protocol. A browser speaks application JSON over its socket;
the proxy's OPEN/TEXT events are a separate server-side concern.

- Client send: `{"type":"send","operation_id":"m-001","text":"hello"}`.
- Application acknowledgment: `{"type":"accepted","operation_id":"m-001","sequence":42}`.
- Delivered event: `{"type":"message","sequence":42,"text":"hello"}`.
- Reconnect request: `{"type":"resume","after_sequence":41}`.

Keep pending IDs across reconnects, deduplicate received sequence numbers, and
only mark a send accepted on an application acknowledgment. A socket write alone
is not acknowledgment. If retained history no longer covers the cursor, require
a fresh application snapshot rather than inventing missing messages. Bound text,
pending sends and replay windows. Authenticate room access; operation IDs are not
credentials. Socket survival across worker changes does not prove replay or
application-level exactly-once delivery. An executable simulator is NH21.
