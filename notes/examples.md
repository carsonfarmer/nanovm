# examples: twenty-two apps on the SDK, each with an end-to-end test

Branch `ideas/examples`, on `cf/onramp` (5884b38). IDEAS section A, built
on `nanovm-sdk` (`sdk/rust`), each in `examples/<name>/` with a README (what
it shows, one run command, the API) and a test in
`crates/nanovm/tests/examples.rs` that runs the built component, checks
what it does and that its state survives eviction and a restart of the
node.

## The examples

| Example | IDEAS | What it shows | Test (s, warm) |
|---|---|---|---:|
| [shortener](../examples/shortener) | A-3 | a link per object; create-if-absent without locks; minting through calls between objects; per-day click counts | 2.2 |
| [ratelimit](../examples/ratelimit) | A-4 | a token bucket per key: 20 concurrent takes on 5 tokens give exactly 5; refill by the clock; limits that survive restarts; `wasi:config` defaults | 4.3 |
| [locks](../examples/locks) | A-16 | one holder however many ask; leases that expire with nothing running; fencing tokens that only grow | 5.2 |
| [polls](../examples/polls) | A-13 | one vote per voter cookie (voting again moves it); live results pushed only for committed votes | 3.4 |
| [leaderboard](../examples/leaderboard) | A-5 | SQLite per board: top-N pages, competition ranks, batches as one statement | 2.7 |
| [shop](../examples/shop) | A-20 | a declined card after the writes rolls back stock, order and cart; check-then-write never oversells; one writing statement via a trigger | 3 |
| [jobs](../examples/jobs) | A-6 | leases whose visibility timeout is a stored time; fenced acks; idempotent enqueue; dead letters; worker objects pulling through calls | 4.2 |
| [events](../examples/events) | A-24 | Server-Sent Events: replay with `retry:`; a streamed body after the handler returns holds its object; a stream past `post_return_timeout_ms` is cut and its later writes roll back | 7.1 |
| [files](../examples/files) | A-25 | uploads under `disk_mb` (507 past it, old file kept); streamed downloads with ranges, ETags, HEAD | 2.1 |
| [site](../examples/site) | A-22 | embedded assets with ETag/Cache-Control and 304s, an API beside them | 2.1 |
| [presence](../examples/presence) | A-12 | join/leave/typing over WebSockets; two tabs are one person; presence that lapses when a restart drops sockets silently | 4.5 |
| [tictactoe](../examples/tictactoe) | A-1 | a lobby pairing players through calls (ten at once make five games of two); each game the authority on its moves | 3.2 |
| [flags](../examples/flags) | A-8 | defaults in config, overrides per tenant object pushed live; stable, monotonic percentage rollouts | 3.3 |
| [wiki](../examples/wiki) | A-10 | revisions in SQLite; lost updates refused with `if-match`; Markdown (`pulldown-cmark`) with raw HTML escaped, link and image destinations limited to http(s), mailto and relative, and a no-script CSP; diffs (`similar`) | 2.2 |
| [notes](../examples/notes) | A-19 | an FTS5 index per person: ranked, highlighted prefix search; user input never query syntax | ~2 |
| [analytics](../examples/analytics) | A-15 | batched ingest in one statement; per-day rollups kept by a trigger; CSV | ~2 |
| [reminders](../examples/reminders) | A-26 | acting on time without alarms: an external tick or client polling, each reminder fired once | 3.3 |
| [todos](../examples/todos) | A-23 | offline-first sync: field-level merges, reported conflicts, retries that apply nothing twice | 2.1 |
| [webhooks](../examples/webhooks) | A-7 | HMAC-verified webhooks answered at once, relayed after the answer (`after`), retried with backoff; egress allowlist | 3.0 |
| [gateway](../examples/gateway) | A-17 | per-user token quotas in front of a streaming API: reserve before streaming, refund after a verified finish (`[DONE]`, body whole), so cut streams aren't free; events parsed across reads | 3.7 |
| [feeds](../examples/feeds) | A-18 | a feed object as a shared cache: conditional GETs, items kept once; readers fan in through calls | 1.6 |
| [oauth](../examples/oauth) | A-14 | GitHub sign-in: one-time state bound to the starting browser by a `__Host-` nonce cookie, token exchange over egress, cookies only the user's object accepts, `auth` vouching for sessions (W-602) | 0.6 |

The first twenty: 72 s in one run, 59 s in the full suite
(`RUST_TEST_THREADS=1`, load average 11-17 on 4 shared cores). Most tests
start `nanovm run` itself; webhooks, gateway, feeds and oauth run on an
in-process dev-mode node (`Local` in the test file), because what they
reach over egress is on loopback and `nanovm run`, like a production node,
refuses private egress.

## Run them

```sh
export PATH=/path/to/wac-0.12/bin:$PATH WASI_SDK_PATH=/path/to/wasi-sdk-34.0-x86_64-linux
for app in examples/*/Cargo.toml; do
  app=$(basename "$(dirname "$app")")
  tools/build-app.sh --locked "examples/$app" -o "examples/target/$app.wasm"
done
ulimit -n 20000; env -u RUST_BACKTRACE RUST_TEST_THREADS=1 cargo test --locked -p nanovm --test examples
```

Each README has its own `tools/build-app.sh … --run` command. CI builds every
`examples/*/Cargo.toml` and runs the SDK's unit tests
(`cargo test --locked --manifest-path examples/Cargo.toml -p nanovm-sdk`).

The harness (`App`) gives each example the most `nanovm run` allows for
`request_timeout_ms` (5 s) and `cpu_ms` (200 ms), and objects an idle
timeout of 300 ms. Eviction is checked by the console's `cold start` line on
the next request; a restart kills the process and waits for it before
starting another (N-601).

## SDK additions (sdk/rust)

Each in its own commit, found by an example:

- `Request::cookie` (polls); unit tests in the SDK and a CI step for them.
- `sqlite`: `query`/`execute`/`last_insert_rowid` on the object's database,
  typed `Row::get`, `Rows::json`; `tools/build-app.sh` composes the SQLite
  provider when an app imports it (leaderboard).
- `Response::stream` and `sse()` (events); a streamed body keeps a
  `content-length` the app sets (files).
- `Dir::rename`, `fs::read_at` (files).
- `after(work)`: work that runs once the handler has answered, committed
  when it is done (webhooks).
- `Fetch::stream` / `Streaming::next`: read an outbound answer as it
  arrives (gateway). `next()` is `Result<Option<_>, ErrorCode>`: a body
  cut short upstream ends in an error, as `send()` returns one, rather
  than passing for the whole answer (review D3).

An app that uses none of them is unchanged: `shortener.wasm` is 400,751
bytes before and after the SQLite module, with no `spin:sqlite` import.

## What the examples found

- **N-600** (open, storage owners): SQLite's own syncs count as CPU. Each
  writing statement is a SQLite transaction whose fsyncs run inline in the
  guest's poll (`allow_blocking_current_thread`), and `QuotaFs` doesn't
  uncharge them as the journal does its own. One-row inserts cost ~6-12 ms
  each at load ~25: 40 in one request ran out of the 200 ms budget; the same
  rows as one `json_each` statement took 25-47 ms for 100. A four-statement
  checkout failed with `cpu` in 1 of 4 runs. The examples write one
  statement per request.
- **N-601** (fixed here): tests restarted `nanovm run` without waiting for
  the old process, which still held the app's storage lock.
- **N-602** (fixed here): `nanovm run` lowered a manifest's limits silently
  (a 4 MiB upload limit became 1 MiB); it now says so.
- **N-603** (fixed here): the SQLite provider's authorizer refused the
  read-only pragma FTS5 asks for, so full-text search failed with
  `access-denied`.
- **W-600**: a request can't roll back and still choose its status: a
  declined card after the writes can only be `500 guest-error`; files writes
  aside and renames to answer `507` with the old file kept.
- **W-601**: a site can't be served at `/` (the first segment is always an
  object).
- **W-602**: a call between objects carries no caller identity, so routes
  meant for other objects are public (tictactoe's `/join`, chat's `/rooms`).
- Also seen, not filed: after the container restart, fsyncs took ~180 ms
  (IO pressure "full" 87%) and the examples' 1-2 s request timeouts turned
  into 504s; the harness now runs at the 5 s maximum. Once, under that
  pressure, a client request failed with a connection error ("error sending
  request") that did not recur in 15 later runs; the harness now prints the
  whole error chain.

## Not done

- **A-21 counter in every language**: only Rust and JavaScript build WASIp3
  HTTP components with the installed tools (C needs `wit-bindgen`'s C
  generator, not installed); the Rust one is `examples/hello`, the
  JavaScript one `js/examples/counter.js`. A table of their overheads needs
  the JavaScript runtime rebuilt (~220 MB) and a quiet machine; not done
  within the disk budget.
- **A-2 collaborative editor** (L, `yrs`), **A-9 pastebin** (`syntect`) and
  **A-11 thumbnailer** (`image`): large dependencies, builds and disk; not
  attempted.

## Pick list

Every commit depends on `cf/onramp` (the SDK, `tools/build-app.sh`, `nanovm
run`). Runtime lines (CI's count of `crates/nanovm/src`): +6, all in
`main.rs` (N-602's warning). `components/sqlite/provider`: +8 (N-603).

## Review fixes (PR #2)

Each with a `review_*` test that failed before it and passes after.

- **D1** oauth: the state is bound to the starting browser by a nonce
  cookie (`__Host-` and `Secure` over https); a callback link opened
  elsewhere signs no one in.
- **D2** wiki: link and image destinations limited to http(s), mailto and
  relative ones; a no-script CSP on rendered pages.
- **D3** SDK: `send()` and `Streaming::next()` report a body cut short
  upstream as an error (JavaScript's `fetch` rejects); the gateway keeps
  the reservation of a stream its upstream breaks off.
- **D4** JavaScript: a rejected `ctx.waitUntil` promise stops the instance
  (as a panic in Rust's `after` work does), so the host rolls back what
  the work wrote after the answer instead of committing it; the answer and
  the handler's own writes, committed when it was sent, stand.
- **D5** gateway: Server-Sent Events parsed across reads (an event split
  between HTTP chunks was zero tokens, so a split stream was refunded in
  full); refunds only after `[DONE]` and a whole body, by the upstream's
  `usage` when it reports one.

## Review 2 fixes (PR #2, `fix/r3-dev`)

Each with a test that failed before it and passes after (both runs below).

### Work after the answer: one model (D4 findings 1-4; N-710, N-711, N-712)

The four findings showed that the semantics of work after the answer had
not been pinned down. The model, now in README "Commits" and followed by
both the JavaScript runtime and the Rust SDK:

1. The answer stands once sent and is never replaced. It commits
   everything written until the node takes it: the handler's writes and
   what background work wrote meanwhile. A waitUntil promise's work starts
   when the promise is made, and a streamed body's writer as the handler
   returns, so what they write before the node takes the answer shares its
   commit and stands if they later fail (finding 2: documented, and its
   test asserts it). Rust's `after` work starts only once the answer is
   out, so it never shares the answer's commit.
2. What is written after the answer is a second commit, made once all of
   the work is done, including work registered meanwhile (`waitUntil` from
   waitUntil work, `after` from `after` work): the set is tracked until it
   is empty (finding 3).
3. Any failure (a panic, a promise that rejects before or after the
   answer, or can never settle, a streamed body's writer that returns an
   error, the deadline) is logged, rolls back the second commit's writes
   and replaces the instance. The answer stands (findings 1 and 4).
4. "Out" means the client has read the answer whole (or has gone). The
   node resolves the future `response.new` gives the guest then; the SDK
   starts `after` work, and the JavaScript runtime's abort happens, only
   after it.

How this differs from the recommended model, and why:

- Point 4 is the addition. A failure must come after the node has taken
  the answer, or it fails the request (the root cause of findings 1 and
  4: the abort ran in the same guest turn as the answer). But "the node
  sent its reply" isn't late enough either: the node aborts a body when
  its instance dies, so a failure while a slow client still reads a long
  body cut the answer. `*_lets_a_slow_client_read_the_whole_answer`
  (Rust and JS: a 1 MiB answer read 200 ms after its headers, work that
  fails at once) shows it. WASI already has the signal: `response.new`
  returns a future of the response's "result of transmission"; the node
  passed the request body's result there, now the body's.
- "Read whole" is when the body guard has given its last frame (or an
  error), or is dropped, not only dropped: a reader may keep a body it has
  read to the end (`gateway_streams_completions_within_quotas` does), and
  the work after the answer then waited for the deadline and rolled back
  (the gateway's refund was lost: `100 reserved, 95 given back`, left 100).
- A rejection before the answer (finding 4) doesn't fail the request, but
  it does fail the work after the answer: the writes after it roll back
  and the instance is replaced, as for a rejection after it. So the
  outcome of a rejection doesn't depend on when it lands; only where the
  answer's commit falls does.

Cloudflare, for comparison (from its documentation as summarised by
search results; developers.cloudflare.com isn't reachable from this
session): in Workers, a promise passed to `waitUntil` that rejects is
reported as an uncaught exception in the logs and affects neither the
response nor other `waitUntil` promises; there is no storage to roll
back. In Durable Objects, `ctx.waitUntil` has no effect (an object stays
alive while it has I/O pending), storage writes are not grouped per
request (the output gate holds a response until the writes before it are
durable), and nothing is rolled back when a promise rejects. Ours logs
the failure as Workers do, holds the answer until its writes are durable
as the output gate does, and is stricter after it: the writes after the
answer are one group that commits only if all the work succeeds.

Before (old runtime, reviewer's tests): never-settles and
rejected-before each `500 trap` with storage `{}`; registered-after
`{"nested":1}` committed; wrote-before `{"early":1}` (now the asserted
behaviour). New Rust tests on the old node and SDK: `/panic-at-once` `500
trap`; `after` from `after` work not run before the next request
(`nested: false`, `served: 2`); `/big-panic` 500. After: all pass.

### A broken upstream stream reaches the gateway's client broken (D3; N-713)

`Response::stream`'s writer returns `Result<()>`. On `Err` the SDK ends
the body with an error in its trailers, which the node turns into a
failed body, so the client sees it break off; then, once the answer is
out, it panics as `after` work, so what the writer wrote after the answer
rolls back (as on a panic). The gateway uses `?` on `Streaming::next`.

### The oauth session cookie is `__Host-` over https (D1; N-714)

As the nonce cookie already was. A plain `session` cookie, which a sibling
host can plant with its own valid session, signs no one in.

### Tests and mutation checks

Full suite (`env -u RUST_BACKTRACE RUST_TEST_THREADS=1 cargo test --locked
-p nanovm`, all 24 examples and the JS test app rebuilt): 155 unit, 9
dev, 27 examples, 98 integration (14 ignored), 17 js, 2 committed_state;
all pass.

Each mutation broke the mechanism, failed a test, and was restored (`git
diff` empty):

| Mutation | Fails |
|---|---|
| SDK: `after` work starts without waiting for the answer to be out | `after_work_that_panics_leaves_the_answer_standing` (`500 trap`), `..._lets_a_slow_client_...` (500), `after_work_passed_on_...` |
| SDK: `after` from work after the answer is queued, not started | `after_work_passed_on_by_after_work_runs_before_the_next_request` (`nested: false`) |
| SDK: a failed writer's error isn't put in the trailers | `a_streamed_body_whose_writer_fails_breaks_off_and_rolls_back` (ended as if whole) |
| SDK: a failed writer doesn't panic once the answer is out | the same (`streamed: true` committed) |
| node: the answer is "out" at once | `after_work_that_panics_leaves_the_answer_standing` (`500 trap`), `..._lets_a_slow_client_...` (500) |
| node: "out" only when the body is dropped, not at its end | `guarded_body_says_when_it_has_been_read` |
| JS: `Promise.all` of the promises registered at the answer (D4's) | `review2_wait_until_registered_after_..._when_it_rejects` (`{"nested":1}`), `wait_until_registered_after_the_answer_is_waited_for` |
| oauth: user objects take a plain `session` cookie | `review2_oauth_session_cookie_is_a_host_cookie_over_https` (planted cookie: 200) |

The JavaScript runtime's abort waiting for the answer to be out is shown by
the before runs (the old runtime spawned it at once): `500 trap` for the
never-settling and the already-rejected promise.

CPU per request was not measured: the node's change is one oneshot
channel and one async wrapper per answered request (the wrapper replaces
the request-body future the node already handed over), and apps without
`after`, waitUntil or a streamed body spawn no new guest task. Disk was too
low for the release builds the method needs (the coordinator asked for
none this round).
