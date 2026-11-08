# Ops notes

Operations and packaging work from IDEAS.md (sections E, F, L, M), one branch per
item, each from `cf/main-next`. Plain language; numbers and how to reproduce.

## F-71 "why did my object die?" (`ideas/errors-guide`)

**What:** `docs/ops/errors.md`: every `x-nanovm-error` reason the code emits
(worker, supervisor, proxy, WebSockets), its status, cause, whether the request's
writes roll back, and what to do; plus where else to look (span `error.type`,
events, node stderr) and the failures that cannot carry a header (truncated
bodies, post-return commits, 431s). `docs/ops/errors-check.sh` triggers them.

**Evidence:** the list was built from every `error_response` call and
`metrics::Poison`. `errors-check.sh` (dev node + proxy, counter and hostile
guests) produced 18 checks covering 17 reasons, each with the expected status and
header; the quoted details are its output. Not triggered: `commit-failed`,
`object-unavailable`, `bad-request-body`, `bad-uri`, the 500 `worker-down` and the
WebSocket refusals.

**Findings:** 4xx reasons never reach span `error.type` (only 5xx do), so
dashboards keyed on it miss client errors by design. At load average ~20, a
request allocating 64 MiB was killed for `cpu` before `memory` (CPU is wall time
in polls); with 40 MiB it hit `memory`. A failed worker start is replayed from
memory for 1 s (`RESTART_BACKOFF`), so a crash-looping app answers 502 quickly
rather than respawning per request.
