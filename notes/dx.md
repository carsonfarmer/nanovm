# dx: from zero to a running stateful webapp

Before this branch, seeing an app answer meant hand-writing raw `wasip3`
bindings, publishing to an OCI registry, starting a dev-mode node and sending
`nanovm-app`/`nanovm-trust` headers with every request. Now it is: write one
async function with `nanovm-sdk`, `cargo build`, `nanovm run app.wasm`, `curl`.

## First five minutes

On this machine (4 cores, shared with other builds; the crates already in
Cargo's cache), following [guests/sdk/README.md](../guests/sdk/README.md)
to the letter, with the SDK as a path dependency because it isn't on `main` yet:

```text
$ cargo new --lib hello && cd hello
$ cargo add --path …/guests/sdk nanovm-sdk     # README: cargo add --git …/nanovm nanovm-sdk
$ printf '\n[lib]\ncrate-type = ["cdylib"]\n' >> Cargo.toml
$ $EDITOR src/lib.rs                           # the 9-line handler from the README
$ cargo build --release --target wasm32-wasip2
    Finished `release` profile [optimized] target(s) in 38.97s
$ nanovm run target/wasm32-wasip2/release/hello.wasm &
nanovm: hello on http://127.0.0.1:8080/<object>/ with data in nanovm-data
$ curl http://127.0.0.1:8080/alice/
alice has had 1 visits
GET /alice/ → 200 in 65.9 ms, cold start
$ curl http://127.0.0.1:8080/alice/
alice has had 2 visits
GET /alice/ → 200 in 21.5 ms
$ curl http://127.0.0.1:8080/bob/
bob has had 1 visits
GET /bob/ → 200 in 22.4 ms, cold start
```

(The `GET …` lines are `nanovm run`'s own output, up to a second after each
request. Each visit is a durable write, fsynced before the answer.) Then
`examples/chat/run.sh` and <http://127.0.0.1:8080/lobby/> for a real app.

## What changed

### `nanovm run`: one command, loopback only

`nanovm run <app.wasm> [--manifest nanovm.json] [--listen 127.0.0.1:8080] [--data DIR]`
(`crates/nanovm/src/main.rs`) builds an ordinary in-process `Node` from the
file and serves it with the node's own `server::handle`, so `/{object}/{path}`
routes, limits, volumes, key-value, config, calls between objects and
per-request commits behave exactly as in a worker. On top it does the three
things a proxy and supervisor would:

- **WebSockets**, with the proxy's own code: an upgrade goes to `ws::Sockets`
  (unchanged), whose events are delivered to `server::handle`. The proxy's
  "open the socket in its own task, answer the upgrade" tail moved from
  `proxy.rs` into `Sockets::upgrade`, shared by both. Publishes needed one
  runtime change: an in-process node used to send `http://nanovm/publish/` to
  its egress policy, which denied it (so in-process nodes, including tests and
  the benchmark, could not publish at all; N-60). Now every node holds
  publishes until its request commits, exactly as a jailed worker does, and an
  in-process node then hands them to its own `Node::sockets`. A publish to any
  other `http://nanovm/` path is now refused with 404 at once instead of
  being accepted and silently dropped by the supervisor (N-61).
- **Logs on the terminal.** Without a Collector the OTLP exporter printed two
  failures a second (N-63). `run` points the node's telemetry at a loopback
  sink of its own that prints each request span and log record: what a
  Collector would receive, as lines such as
  `POST /alice/incr → 200 in 1.3 ms, cold start` and `/alice stderr: …`.
- **Restart on rebuild.** It re-executes itself once the `.wasm` file's mtime
  changes and settles (300 ms polls). A request in flight ends as in a crash,
  so the journal rolls it back; sockets close and the example page reconnects.
  Re-exec, rather than swapping nodes in one process, guarantees an object is
  never live twice on one volume.

It refuses a non-loopback `--listen`. It needs no `dev` feature: it reads no
registry and enables nothing that `serve` could be configured into, and the
app's egress list gets no private-address exemption.

**Why `serve` stays registry-only and signed.** A production node runs code
chosen by whoever controls a DNS name, so it must verify a Sigstore signature
over an immutable digest, jail each app in its own process, and keep no deploy
state; that is what makes it safe to hand out without a long-lived secret.
`run` drops all of that for speed: the guest runs in the CLI's own process
under wasmtime's sandbox only (no uid, seccomp or Landlock), there is no
version drain, and client-sent `nanovm-*` headers reach the guest. That is
fine for a developer's own code on their own machine and wrong for anything
else, hence loopback only. What a developer sees under `run` is what a
default-limits node does with the same component and manifest: limits are
clamped to the node defaults, and routing, commits and sockets are the
production code paths.

Runtime cost (CI's count of `crates/nanovm/src`): 5,346 → 5,462 lines
(+116). `main.rs` +103: argument handling and setup 30, the WebSocket front
14, the telemetry printer 31, restart on change 17, the rest usage text and
imports. The in-process publish path is +10 (`egress.rs` +1, `object.rs`
+7, `table.rs` +2), and moving the socket upgrade into `ws.rs` is +3 net
(`ws.rs` +11, `proxy.rs` −8). `server.rs` is unchanged in size.

### `nanovm-sdk` (`guests/sdk`), replacing `guests/util`

- `nanovm_sdk::serve!(handle)` exports `async fn handle(Request) -> Result`.
  `Result` is `Result<Response, Error>`; any `std::error::Error` converts, so
  `?` works on file, key-value, HTTP and JSON errors, and an `Err` rolls the
  whole request back (the host answers 500 `guest-error` with the message).
  This makes nanovm's commit rule the natural one in Rust: a failed step can't
  leave half a change behind.
- `Request`: `method`, `path` (within the object), `query`, `query_or`,
  `header`, `headers`, `body`, `text`, and `json` with the `json` feature.
- `Response::text/html/json/bytes/new/not_found` with `.status()`,
  `.header()`, `.body()`.
- `data()` and `tmp()`: `read` (an `Option`: a missing file isn't an error),
  `read_text`, `write`, `append`, `remove`, `open`; `fs::write_at`, `fs::code`.
  There is deliberately no "atomic write": every `/data` change is already
  all-or-nothing per request (the undo journal), so write-to-temp-then-rename
  only costs an extra file and journal record. The counter's `/incr` now
  writes in place; the README's bug note about it is updated.
- `kv::get/set/delete/increment/keys` (default bucket), plus the raw
  `wasi:keyvalue` bindings; `config::get`.
- `fetch(url)` builder with `.method/.header/.body`, `.send()` (whole answer)
  or `.open()` (headers only, body held); `object(id).get/post(path)` for
  calls between objects.
- `ws::events(&req)`, `ws::Answer` (`accept`, `subscribe`, `text`, `binary`,
  `close`), `ws::publish(channel, msg)`/`publish_all`.
- `id()`, `app()`, `env()`, `sleep()`, `quote()` (a JSON string, for apps
  without serde), and `wasip3` re-exported for anything else.
- Dependencies: `wasip3` 0.9 (WASI 0.3.0, what wasmtime 50 hosts) and
  `wit-bindgen` 0.62, as before; `serde`/`serde_json` only with `json`. An
  in-house JSON writer was tried and dropped for serde_json behind the feature
  (an existing library, and 120 fewer SDK lines).

Every crate that used `util` now uses the SDK: `counter`, `hostile`, the
post-return test probe, the SQLite provider and example, and the SQLite
experiment crates (their lockfiles changed only `util` → `nanovm-sdk`).

**Guest code** (lines / nonblank non-comment lines):

| | before | after |
|---|---:|---:|
| `counter` | 175 / 151 | 102 / 87 |
| `hostile` | 414 / 359 | 302 / 249 |
| `util` → `sdk` | 287 / 236 | 769 / 502 |
| total of the three | 876 / 746 | 1,173 / 838 |
| every crate built on util/SDK (adds SQLite provider, example, experiments, probe) | 2,281 / 2,069 | 2,548 / 2,130 |

App code shrank by a third; the SDK grew, because it now covers what each
guest used to hand-write or couldn't do (outbound fetch, calls between
objects, WebSocket events and publishes, key-value helpers, the handler
macro and error type, docs). Total guest code did not shrink: counter,
hostile and the SDK are 838 code lines against 746 (+12%); across every
crate that used `util` it is 2,130 against 2,069 (+3%). The two test guests
use only part of the SDK; the chat example (100 lines of Rust) uses most of
it.

Component sizes (release, wasm32-wasip2, built as CI builds them):
`counter` 190,243 → 197,800 bytes (+4%), `hostile` 231,085 → 230,910,
`chat` 253,041.

**Benchmark** (same `bench --quick` binary, baseline guests vs SDK guests,
three alternating pairs, load average 16–18 on 4 shared cores, so noisy;
medians): cold starts 1,368 vs 1,351/s and 117 vs 116 KB per object; warm
GET (256 concurrent) 15,389 vs 18,577/s; one-client GET 21,904 vs
25,144/s; POST `/incr` 274 vs 331/s and one-client p50 36.0 vs 26.1 ms;
churn 826 vs 949/s. The bench process used 19.3–19.6 s CPU per run with
the SDK guests against 22.7–23.5 s with the baseline. No regression; the
`/incr` gain is the in-place write (one file and fewer journal records
instead of a temporary file and a rename). To reproduce: build the bench
(`cargo build --locked --release -p nanovm --example bench`) and both guest
sets (`git archive 491e790 guests | tar -x -C base`, then
`cargo build --release --target wasm32-wasip2` in `base/guests` and in
`guests`), copy each binary out of `target/`, and three times in turn copy
one set's `counter.wasm` and `hostile.wasm` into
`guests/target/wasm32-wasip2/release/` and run
`bench --quick --json <set>-<i>.json` under `time`.

### Showcase: `examples/chat`

A live multi-room chat (`examples/chat/src/lib.rs`, 100 code lines; a
95-line page in `index.html`). Rooms are objects whose history is a JSON-lines file in their
volume; members are WebSockets that get the history on open and every
message after; the `lobby` object indexes rooms, told of each message by a
call between objects, and pushes changes to its own sockets. One command:
`examples/chat/run.sh`.

## Evidence

`run::the_chat_example_is_live_and_durable` starts the real `nanovm run`
binary on `chat.wasm` (with the example's manifest, `idle_timeout_ms`
lowered to 300 so rooms are evicted between steps), then: the lobby page is
served; two sockets in `general` both get a message one of them sends; a
message posted over HTTP reaches both; an empty one is refused (400); the
lobby's `/rooms` shows the room with 2 messages (it learned through a call
between objects), and a lobby socket gets the count 3 live; after 1 s idle
the room's next request is a cold start (the eviction happened) and its
history is intact, and both sockets, open across the eviction, still get the
next message; after killing and restarting the process a new member gets
all four messages as history and the lobby still counts 4; forged lobby
entries, bad room names (`<b>`, `_x`, `.x`) and a 2,001-character message
are refused and nothing is stored. What `nanovm run` printed during one run:

```text
nanovm: chat on http://127.0.0.1:40519/<object>/ with data in …/target/tmp/nodes/chat_example
GET /lobby/ → 200 in 5.2 ms, cold start
POST /general/ws → 200 in 1.3 ms, cold start
POST /general/ws → 200 in 0.7 ms
POST /lobby/rooms/general → 200 in 33.1 ms
POST /general/ws → 200 in 75.8 ms
POST /general/messages → 400 in 0.4 ms
POST /lobby/rooms/general → 200 in 31.8 ms
POST /general/messages → 200 in 66.4 ms
GET /lobby/rooms → 200 in 0.5 ms
POST /lobby/ws → 200 in 0.3 ms
POST /lobby/rooms/general → 200 in 39.6 ms
POST /general/ws → 200 in 57.3 ms
GET /general/messages → 200 in 1.2 ms, cold start
nanovm: chat on http://127.0.0.1:37855/<object>/ with data in …/target/tmp/nodes/chat_example
test run::the_chat_example_is_live_and_durable ... ok
```

A room's message takes 30–75 ms here because it commits twice with fsyncs
(the room, then the lobby's call) on a disk shared with other builds.

## Tests

All on this branch, `--locked`, serial (`RUST_TEST_THREADS=1`), on 4 cores
shared with other agents' builds (load average often 15–23).

- **New tests, all passing:** `run::nanovm_run_listens_on_loopback_only`,
  `run::nanovm_run_serves_requests_sockets_and_publishes`,
  `run::nanovm_run_restarts_when_the_component_changes`,
  `run::the_chat_example_is_live_and_durable`,
  `only_well_formed_publishes_are_held`.
- **Mutation checks** (each change made, test seen failing, file restored and
  compared): no in-process publish hold (`matches!(route, Supervisor)` put
  back) fails two tests; no `/publish` path check fails the publish test;
  no in-process publish delivery fails the sockets test; no event refusal in
  `run` fails it with 200 for a forged event; no loopback check hangs the
  loopback test until its 30 s timeout; no `.`/`_` rule in the chat lobby
  fails the chat test on `_x`.
- **Full suite:** unit 107 passed. Integration 70 passed, 2 failed, 10
  ignored. `kv_reads_and_batches_are_bounded` passed on its own right after.
  `a_node_runs_at_most_max_apps` is a timing flake: on its own it failed 5 of
  7 runs, with the baseline and the SDK `hostile.wasm` alike, including on an
  idle machine after a reboot, and passed the rest (N-64). The runtime binary
  was the same in both arms, and nothing in this branch touches worker
  scheduling, but it was not A/B'd against a baseline runtime build.
- **Opt-in SQLite suite** (`--ignored sqlite_`, fixtures rebuilt from the
  ported crates with WASI SDK 34 and wac 0.12.0): 6 of 8 passed;
  `sqlite_journal_pressure_matrix` passed on a rerun;
  `sqlite_repeated_updates_remain_bounded` goes over its 10 s CPU budget under
  load with the baseline `p3.wasm` as well (4 of 4 alternating runs, N-62).
  `cargo run --example sqlite` (composed provider, rollback, restart): PASS.
- **Not verified end to end:** `examples/chat/run.sh` got through the guest
  build and into the release build of `nanovm` when the container rebooted.
  The command it runs is what the chat test runs (debug binary).

## For the orchestrator

- SQLite fixtures: WASI SDK 34 is at
  `/home/user/wt/dx/target/wasi-sdk-dl/wasi-sdk-34.0-x86_64-linux` (set
  `WASI_SDK_PATH`), and wac-cli 0.12.0 is at `/home/user/wt/dx/target/tools/bin/wac`.
  `experiments/sqlite-filesystem/build.sh --compose` rebuilds every fixture;
  the built ones are in `target/sqlite` and `target/sqlite-filesystem`.
- Merging changes `guests/**`, so `.github/workflows/publish-guests.yml` will
  publish new `counter` and `hostile` versions on `main` (ISSUES line 246 says
  to ask the owner first). Their HTTP behaviour is unchanged; the integration
  tests that pin the CI-signed ghcr.io guests use already-published digests.
- `examples/chat` is a member of the `guests` workspace (one lockfile, one
  target, built by CI's existing guest step); it is not in the publish matrix.
- Docs to fold in: PLAN.md line 68/87 and HANDOFF "Layout pointers" still say
  `guests/util`; HANDOFF could mention `nanovm run` for local work and that
  in-process nodes now deliver publishes. README got small corrections
  (Layout, the counter bug note, a short "Build an app" section).
