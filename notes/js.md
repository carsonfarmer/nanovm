# JavaScript apps (branch cf/js)

nanovm apps can now be written in JavaScript, Workers-style. componentize-js
and StarlingMonkey have no WASIp3 HTTP yet, so `js/` is our own runtime: a
Rust guest component that embeds a JavaScript engine and runs the app's
module. It adds **zero runtime lines** and needs **no change to the import
allowlist** in `app.rs` (it imports only `wasi:http`, `wasi:keyvalue`,
`wasi:config`, clocks, random and cli).

## Example

```js
// js/examples/counter.js — each object (URL /{id}/…) has its own count.
export default {
  async fetch(request, env) {
    let n = (await env.kv.get("n")) ?? 0;
    if (request.method === "POST") await env.kv.put("n", ++n);
    return Response.json({ object: env.id, n });
  },
};
```
`js/build.sh js/examples/counter.js counter.wasm` builds the component;
deploy it like any other.

## What works

- `fetch(request, env, ctx)` with `Request`, `Response` (incl. `Response.json`,
  `redirect`), `Headers`, `URL`, `URLSearchParams`, `TextEncoder/Decoder`,
  `JSON`, `atob/btoa`, `structuredClone`, `console` (to the object's log),
  `setTimeout/setInterval`, `queueMicrotask`, `crypto.getRandomValues/randomUUID`.
- `env.kv`: the object's `wasi:keyvalue` store; `get/put/delete/list`, values
  are JSON. Writes commit with the response, like every other change.
- `fetch()` outbound goes through the host, so under the app's egress policy.
- `env.objects.get(id).fetch(path, init)`: calls between objects (cycles get 508).
- `env.config` (the app's `wasi:config`), `env.id`, `env.publish(channel, msg)`.
- WebSockets: `webSocketOpen(ws)` (return `false` to refuse),
  `webSocketMessage(ws, message)`, `webSocketClose(ws, code)`; `ws.send`,
  `ws.close(code)`, `ws.subscribe(channel)` (GRIP).
- `ctx.waitUntil(promise)`: runs after the answer, before the next request.
- Module state lives as long as the instance (like the Rust guest's memory).
- An uncaught exception fails the request (500 `guest-error`, with the JS
  error), which rolls back its writes. Runaway loops and allocations are
  killed by the app's CPU and memory limits like any guest.
- Errors at build time: a syntax error, a missing `export default { fetch }`,
  or top-level code that reaches the host fail `js/build.sh` with the app's
  error.

## How

- **Engine: Boa** (`boa_engine` 0.22, pure Rust). QuickJS (`rquickjs`) needs a
  wasm C sysroot (wasi-sdk), and none is installed here (clang 18 has the
  wasm32 target but no wasi-libc headers), so it was not an option without
  downloading a toolchain. (CI already pins wasi-sdk-34 for SQLite: W-50.)
- `js/runtime/src/prelude.js` (~310 lines of JS) implements the Web APIs on
  seven host functions in `lib.rs` (~520 lines of Rust). `lib.rs` drives the
  engine: promise jobs run until the request's promise settles; host calls
  (outgoing HTTP) and timers are async jobs it awaits properly (Boa's own
  executor busy-polls, which would burn the CPU budget while waiting).
- **Packaging:** `js/build.sh app.js out.wasm` compiles the app into the
  runtime (`include_str!` from `$NANOVM_JS`), then **pre-initializes** it
  with Wizer (`crates/nanovm/examples/js-wizer.rs`, on `wasmtime-wizer` 50
  with no wasmtime features beyond nanovm's own): the component starts with
  the engine up and the module evaluated. During that step every import but
  std's hash seed traps, so no handle, environment, clock or random state can
  be baked in; the runtime reads clocks itself rather than Boa's start-up
  clock. Test `instances_share_nothing_from_the_snapshot` checks it (and
  fails when init is given randomness: mutation-checked).

## Numbers (vs the Rust counter guest)

In-process node, one client, serial; CPU = process user+sys over the phase
÷ requests. Variants alternate round by round; the machine had load average
9–19 from other agents, so latencies are noisy and CPU is the number to
trust. `cargo run --release --example js-bench -- --rounds 3 [name=path.wasm …]`.

| | Rust counter | JS, no pre-init | JS (shipped) |
|---|---|---|---|
| cold start, CPU | 0.39–0.61 ms | 9.4–9.7 ms | 2.4–2.6 ms (min app: 0.9–1.2 ms) |
| cold start, p50 | 0.4–0.6 ms | 13–25 ms | 4–6 ms (min app: ~1 ms) |
| warm GET, CPU/request | 38–48 µs | 274–411 µs | 253–298 µs (min app: 138–191 µs) |
| memory per live object | 62–120 KB | 1.54–1.58 MB | 1.25–1.40 MB |
| component | 190 KB | 4.8 MB | 5.4 MB |
| first compile (cranelift) | 0.4 s | — | 4–11 s, then cached |

- Pre-initialization cut cold-start CPU **3.7×** (9.4 → 2.5 ms).
- Per-request trims (lazy request headers, null empty bodies, one ctx object,
  one kv bucket per instance) cut warm GET CPU ~10–12% (counter 292 → 258 µs
  median; minimal app 196 → 177 µs).
- The JS counter's cold start includes the host creating the kv bucket's
  directories on its first read, a journaled commit (N-110); a JS app
  without kv starts in ~1 ms.
- A profile of warm requests is flat: 67% in the guest (Boa's interpreter
  50%, GC 5.5%, malloc 3.5%), 26% host. No single hotspot is left; the
  remaining gap to Rust is interpretation.
- Tried and dropped (no measured gain): `opt-level = 3` (component 7.7 MB,
  GET within noise), a GC before the snapshot (memory within noise).
- POST (a write and its commit) is fsync-bound for both, ~2–3 ms CPU.

## Tests

`crates/nanovm/tests/js.rs`, 9 tests on a real in-process node with the
built `js/tests/app.js`: routing and storage across eviction (module state
reset), rollback when the handler throws, a promise that can never settle,
CPU kill of `for(;;){}` and memory kill of a doubling string, egress denial
(unlisted host, and a listed name resolving to loopback) and an allowed
round trip, calls between objects and the cycle refusal, Web APIs, timers,
`waitUntil`, config, request headers, WebSocket events, and the snapshot
sharing nothing. Full suite passes (107 unit, 67 integration + 10 ignored,
9 js); the production check passes.

## Gaps

- Speed and size: ~6× the Rust guest's CPU per request, ~10× its memory
  per object, 5.4 MB component. QuickJS would likely do better (W-50).
- Each app relinks the runtime (~1 min, needs Rust); a generic runtime that
  takes the source at pre-initialization would build an app in seconds (W-53).
- Bodies are whole (no `ReadableStream`); one module (bundle imports with
  esbuild); kv values are JSON; no `/data` file API; no alarms (W-52).
- `env.publish` needs the supervisor, so it is tested only through its
  format (the Rust counter's); a supervised JS chat test is a follow-up.
- The binding code (request/response, kv, config) duplicates `guests/util`;
  it should share dx's guest SDK once that lands (W-51).

## For the orchestrator

- CI: `.github/workflows/ci.yml` builds the JS test app after the guests
  (and caches `js/target`); `scripts/build-guests.sh` builds it too. The
  first runtime build takes ~4–5 min (Boa, fat LTO), each app ~1 min + ~7 s.
- `crates/nanovm` gains the dev-dependency `wasmtime-wizer` (only that crate
  compiles; no wasmtime feature changes).
- Docs to fold in: a README section "JavaScript apps" (this file's example
  and "What works"); PLAN "Guests" gains `js/`; ISSUES W-6 (JS and Python
  guests): JS done on this branch with our own runtime, Python open.
