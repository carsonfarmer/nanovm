# Hardening (`cf/hardening`)

Four production hardening issues found by the quality and tooling agents,
plus one bug found while fixing them. Each is one commit with a test that
fails before and passes after.

| Issue | Commit | What changed | Runtime lines |
| --- | --- | --- | --- |
| N-506 | cc0aca8 | the node refuses a `nanovm-host` that isn't a DNS name | +3 |
| N-671 | 2310766 | `preflight` explains an address-space limit; `nanovm serve` runs it | +15 |
| N-500 | 59c0b58 (after merging `ideas/deploy-validation`, dff4327) | the linker defines only the WASIp2 the allowlist permits | +12 |
| N-680 | af293c7 | a link connection is pooled only when idle | 0 |
| N-505 | 50ef6d9 | request bodies must keep coming, at the proxy and the node | +66 |

Runtime lines are CI's count (`.github/workflows/ci.yml`): 5,458 at
`cf/main-next` 5a9fe61, 5,554 after (+96). The commit messages of N-506,
N-671 and N-500 say +4, +17 and +17; the counts above are the right ones.

Measurements were made with the dev profile (dependencies at opt-level 3,
nanovm at 1) on the shared 4-core VM while other agents built (load 2–5), so
they compare old and new builds run alternately, not absolute speed.

## N-505: slow request bodies

**What was wrong.** Only a request's headers had a deadline (10 s, at the
proxy). A client could send its body a byte at a time and hold, for as long
as it liked (up to `max_request_body_kb`), a proxy connection, a supervisor
connection, a link to the worker with its permit (and the app counted as
busy, so its slot couldn't go to another app), and a worker task.

**The rule.** A request body's sender may keep the server waiting 10 s in
all, plus one second for every 4 KiB it sends. Past that the request is
answered `408` with `x-nanovm-error: body-timeout`, and the connection closes
(the body wasn't read to the end). So:

- a body trickled a byte at a time is cut after about 10 s;
- an upload at 4 KiB/s or faster is never cut, however long it takes (1 MiB
  at exactly 4 KiB/s takes 266 s); slower than that, a client still gets
  some way (at 2 KiB/s, about 40 KiB);
- only time spent waiting for the sender counts. Time the server spends not
  reading the body (a worker starting, the socket to the next hop full,
  before it first asks for a `100-continue` body) isn't charged to it.

Where: `server::Paced` wraps the body where the node reads it
(`server::handle`, so every worker and in-process node), and where the proxy
forwards it, so the proxy holds even if its node doesn't read. Whichever hop
notices first answers; the answer is the same. Responses are untouched: a
streamed response (SSE, a download) is bounded by `post_return_timeout_ms`
as before, and WebSockets are upgrades with no body, held by the proxy.

**Evidence.** `slow_request_bodies_are_cut_at_every_entry_point` trickles one
byte every 200 ms at four entry points at once: an in-process node, the
supervisor, the proxy, and a proxy whose node takes requests and never reads
them. Each got its 408 after 10.001–10.007 s in 5 of 5 runs. Before the
change every one was still open, with no answer, after 40 s. Meanwhile 128
KiB sent at 8 KiB/s through the proxy took 16 s and was stored (204, read
back whole).

**Cost.** CPU per request of the supervisor plus the worker (from `/proc`),
`oha` with 32 connections against the counter app, 8 rounds alternating old
and new builds, median (range):

| Request | before | after |
| --- | --- | --- |
| GET | 151.0 µs (129.5–160.0) | 151.7 µs (146.0–159.2) |
| POST, 1 KiB body | 147.8 µs (139.5–151.8) | 152.4 µs (146.2–156.5) |
| POST, 1 KiB body (10 more rounds) | 151.0 µs (142.5–160.8) | 155.5 µs (144.8–165.2) |
| POST, 64 KiB body | 286.5 µs (270.0–294.0) | 282.5 µs (257.0–305.0) |

The differences are inside the noise. A microbenchmark of the wrapper alone
(1M bodies, 3 rounds) bounds its real cost: 5–9 ns for a body that is ready
when first read, 271–290 ns for one that makes its reader wait once (the
timer is set up and torn down).

**Found on the way: N-680.** Through the supervisor, 1 run in 5 got its 408
and then the connection stayed open. The worker had answered before reading
the whole body, and the supervisor's link connection, still sending that
body, went back into the idle pool when the response ended, where nothing
polls it. It held the client's body and connection for good. A connection is
now pooled only when it is idle (`SendRequest::is_ready`, which `checkout`
already required, so such a connection was never reused anyway). The same
could happen after a 413. `a_connection_answered_before_its_body_is_sent_is_not_pooled`
fails every time without the fix.

Reproduce:

```sh
ulimit -n 20000
env -u RUST_BACKTRACE cargo test --locked -p nanovm --lib server:: link::
env -u RUST_BACKTRACE cargo test --locked -p nanovm --test integration slow_request_bodies -- --nocapture
```

## N-500: the linker defined all of WASIp2

**What was wrong.** `app::linker` added every WASIp2 interface, including
`wasi:filesystem@0.2`, which shares the instance's read-write `/data` and
`/tmp`, and `wasi:sockets@0.2`. A guest using `std::fs` linked; only the
import allowlist, checked by name at load, kept its writes from bypassing
the quota and the journal.

**What changed.** The linker adds WASIp2 io, clocks, `random` and stdio
(wasmtime's proxy-world set), then `random`'s insecure and insecure-seed and
the rest of cli. That is exactly the WASIp2 part of `ALLOWED_IMPORTS`; the
p3 filesystem, http, config and p3 cli/clocks/random are unchanged.

**Evidence.** `ideas/deploy-validation` is merged in, and its test now
asserts that `p2_sockets` and `p2_filesystem` fail to link against
`app::linker`, naming `wasi:sockets/…` and `wasi:filesystem/…`, while the
control links. Before: "p2_sockets links against app::linker". Its other
test (a jailed worker refusing a sockets component) and the KV and config
tests pass.

**Cost.** Building the linker, once per app load: 148 µs → 90 µs median
(112–252 vs 85–128, 9 runs each). `instantiate_pre`: 15 µs both. First
request to a new object (1,000 per run): 0.8–1.5 ms both, with pairwise
differences that follow which build ran second, not which build it was.
Instantiation doesn't use the linker.

## N-506: a `nanovm-host` starting with `.`

`AppRef::from_headers` now accepts an empty host or DNS labels (letters,
digits, `-`, `_`; 1–63 bytes each; at most 253 bytes in all) and refuses the
rest as a bad app reference (400). Every accepted host gives a valid app
name. `registry::tests::hosts_are_dns_names` fails without the check on
`.com`, whose app name was `.com_ghcr.io_…`.

## N-671: `ulimit -v`

The pooling allocator reserves every slot's address space when the engine is
built. Under an address-space limit the supervisor started and every app's
worker then failed with "failed to create memory pool mapping". `preflight`
now compares `RLIMIT_AS` with that need, and the supervisor runs `preflight`
too (its workers inherit its limits):

```
max_live_objects = 1000 with memory_reservation_mb = 128 reserves about 522 GiB of address space,
but RLIMIT_AS is 65536 MiB; remove the limit (`ulimit -v unlimited` or LimitAS=infinity in the unit)
or lower max_live_objects or memory_reservation_mb
```

The need is `(max_live_objects + 32) × (4 × (memory_reservation_mb + 64 KiB)
+ 4 MiB) + 1 GiB`. Measured by building the node's engine under `prlimit
--as` (binary search, 16 MiB steps):

| Slots | Reservation | Fails below | Memories alone | Check asks |
| --- | --- | --- | --- | --- |
| 33 | 128 MiB | 17,034 MiB | 16,904 MiB | 18,060 MiB |
| 33 | 64 MiB | 8,587 MiB | 8,456 MiB | 9,612 MiB |
| 132 | 128 MiB | 68,071 MiB | 67,617 MiB | 69,169 MiB |
| 1,032 | 64 MiB | 267,834 MiB | 264,450 MiB | 269,602 MiB |
| 1,032 | 128 MiB | 532,031 MiB | 528,642 MiB | 533,794 MiB |

So beyond its memories a slot needs about 3.26 MiB (two 1 MiB stacks, eight
160 KiB tables) and the bare engine about 24 MiB; a real worker needs more
for its threads and code, hence the 1 GiB.
`serve_explains_an_address_space_limit` runs `nanovm serve` under `ulimit -v`
4 GiB and expects the message; before, serve started and the test failed
after 10 s.

## Open risks

- **Slow uploads.** Clients that sustain less than 4 KiB/s lose bodies over
  about 40 KiB. A client paying 4 KiB/s per connection can still hold one
  for 10 s plus a second per 4 KiB (266 s for 1 MiB); per-client limits in
  the proxy (G-75) are the answer to many such connections. The
  supervisor's drain still allows a request only `request_timeout_ms +
  post_return_timeout_ms`, so a switch cuts a slow upload as before.
- **The node's front end has no header timeout** (N-681, open): a client
  that reaches the node directly can hold a connection with a slow request
  head. Behind the proxy, as deployed, it can't.
- **N-506**: the proxy doesn't check owner names itself, so a DNS answer
  with an owner that isn't a DNS name (never seen) would be refused per
  request at the node (400) rather than at the proxy.
- **N-671**: the 1 GiB slack refuses limits between the real need and the
  need plus 1 GiB. A supervisor whose fd or mapping limits are too small for
  `max_live_objects` now refuses to start, where before only its workers
  failed.
- **ISSUES.md**: the N-505, N-506 and N-671 entries live on
  `ideas/quality` and `ideas/tooling`, not on this branch's base; mark them
  fixed here when merging. N-500 came with the merge and is marked fixed.
