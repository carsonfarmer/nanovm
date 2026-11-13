# Why did my object die?

When nanovm itself answers a request with an error, the body is
`{"error": "<reason>", "detail": "<why>"}` and the same reason is in the
`x-nanovm-error` response header (`object::error_response`). An error your
guest returns itself (say the counter's `507` when its disk quota is full) has
no such header: nanovm passed it through, and committed what the request wrote.

Where else to look:

- **Spans.** A 5xx sets the span's `error.type` to the reason
  (`telemetry::response`) on the layer that answered: `request` (the object),
  `supervisor.request` (the node) or `proxy.request`. 4xx reasons are only in
  the header and body; the span has the status code.
- **Events.** The worker logs `event.name` = `cpu`, `memory`, `timeout`, `trap`,
  `resource-table`, `instantiate-failed`, `rolled-back` or
  `post-return-commit-failed`, with the object id and the detail, correlated
  to the request's trace.
- **Node stderr.** `nanovm[<app>]: …` lines are a worker's own stderr, the last
  one being why it failed to start; `nanovm: worker for app "…" exited: …` is a
  worker ending.

## The object's own failures (worker)

An instance that fails is discarded: its memory is gone, and what the request
wrote to `/data` (files, KV, SQLite) **rolls back**. The next request starts a
fresh instance. The `detail` names the limit; limits come from the app's
`nanovm.json`, clamped to the node's `[limits]`.

| Status | `x-nanovm-error` | What happened | What to do |
|---|---|---|---|
| 500 | `cpu` | Used more than `cpu_ms` of CPU in one request ("used more than 100ms of CPU in one request"). It is the CPU time of the thread polling the guest (N-640), so waiting for a core or the disk isn't charged; the journal's own work isn't either. | Do less per request, or raise `cpu_ms` (app, within the node's ceiling). A slow host shows as `timeout`, not `cpu`. |
| 500 | `memory` | Tried to grow linear memory past `memory_mb` ("tried to grow linear memory past 32MiB", then the guest's own stderr tail). | Raise `memory_mb`, or hold less. Memory doesn't survive a kill or eviction; keep state in `/data`. |
| 504 | `timeout` | The request took longer than `request_timeout_ms` ("deadline exceeded while handling"). | Don't wait on slow upstreams inside one request; raise the timeout. In post-return work (`post_return_timeout_ms`) the response was already sent: only the span records it, and the post-return writes roll back. |
| 500 | `trap` | The guest trapped: a panic, `unreachable`, out-of-bounds, stack overflow. The detail has the trap and the tail of the guest's stderr (the panic message). | A guest bug: read the stderr tail. |
| 500 | `resource-table` | Too many live handles (`max_resources`, also at most `memory_mb × 16`): usually leaked streams, bodies or fields. | Drop handles you're done with. |
| 500 | `guest-error` | The handler returned a `wasi:http` error code instead of a response (the detail is the code). The request rolls back and the instance restarts. | Return a response (any status commits); return an error code only to roll back. |
| 500 | `commit-failed` | The host could not make the request's changes durable: an I/O error from the disk (full, read-only, failing; "journal requires recovery" once a full disk refused a journal record), or, with `[capture]` on, the object's history here is full (`max_versions` or `max_mb`; detail "no storage space"): capture refuses further writes rather than drop versions, and with a `store` that is down, versions wait here until it is back. With `remote_ack`, "version N is not in the store": the store didn't name the version within 2 s; the commit still stands here. A client can't always know whether a commit landed. | See the disk runbook. For capture, bring the store back or raise the limits; never prune `~committed/` by hand. Retry with an operation id. |
| 503 | `instantiate-failed` | A fresh instance could not be created, e.g. `memory_mb` below the component's own minimum ("memory minimum size of 17 pages exceeds memory limits"), its CPU budget ran out while instantiating ("cpu budget exceeded"), or `[capture]` was removed from the config of a node whose objects have history ("committed history requires capture to remain enabled"). Its journal recovery failing also lands here. Nothing ran. | Raise `memory_mb` or `cpu_ms`; put `[capture]` back. |
| 503 | `object-busy` | The object's queue is full: `max_queue` requests already waiting behind the running one ("queue full (64 waiting)"). Nothing ran. | Retry with backoff; a hot object serves one request at a time, so shard it. |
| 503 | `node-at-capacity` | `max_live_objects` objects are live and none idle to evict ("1000 live objects, none idle"). Nothing ran. | Retry with backoff; raise `max_live_objects` (with its fd and mapping needs) or add capacity. |
| 503 | `object-unavailable` | The object's actor stopped before answering (rare: the node shutting down, or the object retired with the request queued). | Retry; with an operation id if the request writes. |
| 508 | `call-cycle` | Only a *calling* guest sees this, on a call to `http://nanovm/objects/<id>/…` that would wait on itself directly or through other calls ("h1 is already waiting on h1"). | Don't call back up the chain: one request at a time per object would deadlock. |
| 413 | `request-too-large` | The body is over `max_request_body_kb` ("max_request_body_kb is 1024"). | Send less, or raise it. |
| 408 | `body-timeout` | The client sent the body too slowly: more than 10 s of waiting in all, plus 1 s per 4 KiB sent ("the request body came too slowly"). The proxy applies the same rule, with the same reason. The connection is closed. | A client problem, or a very slow link. |
| 400 | `bad-request-body` | Reading the body failed (the client went away, a malformed chunk). | A client problem. |
| 400 | `bad-uri` | The path and query don't make a URI with the request's host. | A client problem. |
| 400 | `bad-object-id` | The object id isn't `[A-Za-z0-9_.-]{1,128}`, or starts with `.` or `_`. Also from the supervisor and the proxy (WebSockets). | Use another id. |
| 404 | `not-found` | A `/_nanovm/` route that doesn't exist. | — |

Not visible as a status: a response body over `max_response_body_kb`, or an
instance killed after its headers were sent, ends the body early (the client
sees a truncated body or a reset). A failed post-return commit is recorded
only as `error.type = post-return-commit-failed` on the span and an event.

## The node (supervisor)

| Status | `x-nanovm-error` | What happened | What to do |
|---|---|---|---|
| 502 | `worker-down` | The app's worker isn't there. The detail says which case: a link error (the worker died with the request in flight: its writes roll back unless it had committed, so the outcome is unknown); `loading <app ref>: …` (registry unreachable, digest unknown, **not signed by a trusted identity**, version below the floor, a blob too big, registry on a private address); `worker did not start (…): <its last words>` (preflight: fd or mapping limits; jail: Landlock, uid, a directory it can't reach); `the node runs its most apps (100), none idle`; `the app was restarting; try again`; `the app's secrets, from the node's secrets directory: the app needs secrets <keys>; <app>.json doesn't give them` (or the file isn't a JSON object of strings, or isn't a file: [secrets](../../deploy/single-node/README.md#6-app-secrets); never a value). A failed start is answered from memory for 1 s, then tried again. | Read the detail and the node's stderr; fix the cause (see the worker crash-loop runbook). Retry writes with an operation id. |
| 500 | `worker-down` | The task starting the worker panicked: a bug. | Report it with the node's stderr. |
| 400 | `bad-app-ref` | The `nanovm-app`/`nanovm-trust` headers are malformed ("nanovm-app must name a digest", "nanovm-trust is required", "nanovm-trust entries are `<issuer> <subject>`"). Only the proxy should send them. | Check the proxy, or your test client. |
| 400 | `no-app` | No `nanovm-app` header: the request didn't come through the proxy. | Go through the proxy. |
| 421 | `not-held` | Several nodes only (`leases`): another node holds the object's lease. The proxy tries that node; a client never sees this through it. | — |
| 404 | `not-found` | An unknown `/_nanovm/` route. | — |

## The proxy

| Status | `x-nanovm-error` | What happened |
|---|---|---|
| 404 | `proxy` | `no app at <host>`: no TXT record at `_nanovm.<host>`. |
| 502 | `proxy` | The record couldn't be used: the DNS-over-HTTPS resolver failed or timed out, more than one TXT record, a malformed one (`app=` must name a tag, not a digest; `trust=` is required; an unknown or repeated field), the tag couldn't be resolved at the registry, or no node could be reached within `failover_ms`. The detail says which. |
| 504 | `proxy` | A node took the request but didn't answer within `failover_ms` ("the request may have run there"); the proxy never sends it again. |
| 400 | `proxy` | A client sent a WebSocket event itself ("socket events come from the proxy"). |
| 400, 426, 431, 503, 502 | `socket-refused` | A WebSocket open refused: not a handshake (400), not version 13 (426), headers too large (431), `max_sockets` for the proxy, the app or the object (503), or the object refused it or answered `OPEN` with something a socket can't take (502). |

Requests whose head is over 64 KiB get a plain 431 from any hop, and a client
that doesn't send its headers within 10 s is disconnected; neither has the header.
Outbound calls never pass an upstream's `x-nanovm-error` on as nanovm's.

## How this was checked

Every reason above is from an `error_response` call in `crates/nanovm/src`
(`object.rs`, `server.rs`, `table.rs`, `egress.rs`, `supervisor.rs`,
`proxy.rs`, `ws.rs`, `table.rs`), and the poison reasons from `metrics::Poison` and
`object::classify`. [errors-check.sh](errors-check.sh) triggers 18 of them (20 checks, two of them with local capture) against
a dev-mode node and proxy, with the counter and hostile guests, and prints
status, header and detail; the details quoted above are its output, again on
round 3 (`beta/deployable`, notes/beta-deployable.md). Not
triggered there: `commit-failed` from the disk or the store, `object-unavailable`,
`bad-request-body`, `bad-uri`, `body-timeout`, `not-held`, the proxy's 504,
`500 worker-down` and the WebSocket refusals (covered by the integration tests,
not here).
