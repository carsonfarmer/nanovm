# JavaScript on QuickJS (branch cf/qjs)

**Decision: JavaScript apps now run on QuickJS (QuickJS-NG through `rquickjs`
0.14) instead of Boa.** Against the same app, pre-initialized the same way,
QuickJS is better on every measure: half the CPU per warm request, a quarter
of the component size, a quarter less private memory per live object, and an
app builds in about 10 seconds instead of 45. Boa is removed. The app
API is unchanged, the import allowlist is unchanged, and `js/` still adds
zero runtime lines. QuickJS executes a warm counter request in 3.7× fewer
instructions than Boa; the host's fixed share of each request keeps the CPU
gain at 2×.

## Numbers

The JS counter (`js/examples/counter.js`) against the Rust counter guest,
in-process, one client, serial requests. CPU is the process's user+sys over a
phase ÷ requests. Variants alternate round after round (6 rounds; machine
load average 8–12 from other agents, so CPU is the number to trust and the
ranges are wide). Median (range):

| | Rust counter | **QuickJS** | Boa (before) |
|---|---|---|---|
| warm GET, CPU/request | 128 µs (83–151) | **218 µs (206–239)** | 438 µs (418–467) |
| warm GET, instructions/request | 369k | **428k** | 1566k |
| warm POST, CPU/request | 1.06 ms | **1.18 ms** | 1.64 ms |
| cold start, CPU | 0.48 ms | **2.1 ms** | 2.5 ms |
| memory per live object, private | 50 KB (34–78) | **257 KB (228–268)** | 352 KB (337–366) |
| memory per live object, RSS | 62 KB | **633 KB** | 1298 KB |
| component | 190 KB | **1.5 MB** | 5.4 MB |
| runtime build from scratch (wall / CPU) | — | **94 s / 90 s** | 271 s / 235 s |
| each further app (relink + pre-init) | — | **6.5 s + 3 s** | 38 s + ~7 s |

- A second run right after a container restart (4 rounds; the machine was
  idle at the start and busy again by the end) agrees: warm GET 75 / 156 /
  374 µs (Rust / QuickJS / Boa), private memory per object 45 / 259 / 350 KB.
  Its cold-start and POST numbers are dominated by fsyncs on cold disk caches.
- Build times are one fresh `cargo build --release --target wasm32-wasip2`
  of each runtime with the counter, then the rebuild for `js/tests/app.js`,
  run back to back at the same load.
- **Instructions per request** come from valgrind: `(I refs of 1200 GETs −
  I refs of 200 GETs) ÷ 1000`, host and guest together. They don't move with
  machine load (repeat runs agree to 0.1%), so they are what the per-request
  trims below were judged on.
- **Cold start** is mostly the host creating the kv bucket's directories with
  a journaled commit on the first `env.kv.get` (N-110, not JS); a JS app that
  doesn't touch kv starts in ~0.7 ms of CPU.
- **Memory:** RSS overstates JS objects (N-143). Each instance maps the
  component's memory image from the compiled file, and every page an instance
  merely reads is counted again in RSS although all instances share it.
  `js-bench` now also prints private memory per object; per `/proc/self/smaps`
  a QuickJS object's private memory is ~220 KB of snapshot heap pages its first
  request writes (reference counts, allocator metadata) plus ~80 KB of
  instance state, about what a Rust object has (W-67).
- Tried and dropped (no measured gain): QuickJS at `-O2` instead of `-Os`
  (+320 KB, GET within noise), a GC before the snapshot (private memory
  208.9 vs 208.5 KB).

Commands (the orchestrator can rerun them on an idle machine):

```sh
export WASI_SDK_PATH=/path/to/wasi-sdk-34.0-x86_64-linux
js/build.sh js/examples/counter.js /tmp/qjs-counter.wasm
# baseline: Boa, from cf/optimistic-feynman-62f1a0 (js/build.sh there needs no WASI SDK)
cargo run --release -p nanovm --example js-bench -- --rounds 6 --cold 100 --warm 3000 --posts 300 \
    rust=guests/target/wasm32-wasip2/release/counter.wasm qjs=/tmp/qjs-counter.wasm boa=/tmp/boa-counter.wasm
# instructions per GET, for each variant (run once first so the module is compiled for valgrind's CPU):
valgrind --tool=cachegrind --cache-sim=no --cachegrind-out-file=/dev/null \
    target/release/examples/js-bench --rounds 1 --cold 1 --warm 200 --posts 1 x=/tmp/qjs-counter.wasm   # and --warm 1200
```

## Per-request trims (judged on instructions per GET)

| step | counter | minimal app |
|---|---|---|
| first QuickJS version (prelude as for Boa) | 551k | 408k |
| kv values parsed and stringified in Rust (`JS_ParseJSON`), `Response.json` without a second `Headers` and an object spread, `TextDecoder` without argument spreading | 445k (−19%) | — |
| `Body` as a mixin instead of a base class (no derived constructor per `Request`/`Response`), and no async function in the dispatcher: the runtime waits on the app's promise itself and calls `answer(response)` after | 428k (−4%) | 329k (−19%) |

For scale: the same component answering from Rust without running any JS
costs 161k, and the Rust counter 369k (its file read is dearer than a kv get).

## What changed for JavaScript developers

- **Same API**: `export default { fetch(request, env, ctx) }`, `env.kv`,
  `fetch`, `env.objects`, `env.config`, `env.id`, `env.publish`,
  `ctx.waitUntil`, the WebSocket handlers, timers. The language is QuickJS-NG's,
  which covers modern JavaScript (modules, top-level await, private fields and
  methods, …).
- **Building needs WASI SDK 34** (QuickJS is C): `WASI_SDK_PATH=… js/build.sh
  app.js app.wasm`. CI installs it for the JS step as it already did for
  SQLite. Once the runtime is built, an app takes ~10 s instead of ~45 s.
- **Web APIs** that Boa's runtime library supplied are now the prelude's:
  `URL` (parsed by the `url` crate, WHATWG; setters, `canParse`, `parse`, and
  a live `searchParams`), `TextEncoder`/`TextDecoder` (UTF-8; `fatal`,
  `ignoreBOM`), `structuredClone` (plain data, Date, RegExp, Map, Set, binary),
  `console` (log, info, debug to the object's stdout; warn, error to its
  stderr; objects as JSON), `setTimeout`/`setInterval` and their `clear…`.
  QuickJS brings `atob`/`btoa`, `queueMicrotask` and `DOMException`.
- **Uncaught errors carry their stack** in the 500's detail and the log
  (`Uncaught Error: boom\n    at …`).
- **Differences:** `console` has only those five methods (Boa's had `time`,
  `count`, `table`, …); there is no `performance` (W-65); `kv.put` of something
  JSON can't hold (a function, a symbol) throws a `TypeError` (before, it
  stored an empty value that a later `get` failed to parse); runaway recursion
  fails the request as a trap instead of a `RangeError` (N-140); a string doubled in a loop hits QuickJS's string
  length limit (a catchable `InternalError`) before the memory limit, while
  real allocations still hit the memory limit and kill the instance.

## How

- `js/runtime/src/lib.rs` (~510 lines; Boa's was ~520) is now a guest-SDK
  app (W-51): it uses `nanovm_sdk::serve!`, `Request`, `Response`, `fetch`,
  `kv` and `config` instead of its own wasi:http and keyvalue code. The SDK gained
  `Request::url()` (the whole URL as received) and now drops a `content-length`
  an app set itself, so a response never carries two.
- `prelude.js` (~420 lines; Boa's was ~310) builds the Web APIs on 14 host
  functions. Host calls that wait (outgoing HTTP, timers) are Rust futures the runtime polls
  while JavaScript has nothing to run; each, when done, settles its promise or
  calls its timer back. A request whose promise can't settle any more fails
  with "can never settle", as before.
- **Snapshot guarantees** (pre-initialization with Wizer, unchanged tool):
  every import still traps at build time except std's hash seed, which now
  keys only rquickjs's internal map of Rust types, never app data (N-111).
  QuickJS reads the clock when it starts a context to seed `Math.random`
  (N-142): the runtime's own `gettimeofday` answers 0 only during that call
  (any other build-time read traps, so top-level `Date.now()` still fails the
  build), and `Math.random` is a generator seeded from the host at its first
  use. Test `instances_share_nothing_from_the_snapshot` fails without that
  replacement (mutation-checked: both instances drew 0.6711372530266764).
- **Limits:** QuickJS allocates through the runtime's malloc wrapper, which
  traps when memory runs out. With plain malloc QuickJS turned the memory
  limit into a catchable "out of memory" exception, and the instance
  survived (mutation-checked: `/alloc` answered `guest-error`, not `memory`).

## Tests

`crates/nanovm/tests/js.rs`, the same 9 tests on the QuickJS build, extended:
`web_apis` now checks `URL` resolution, parts, setters and the live
`searchParams`, `TextDecoder` BOM and `fatal`, `structuredClone` of a Map of
Dates, `atob`, `setInterval`/`clearInterval`, and that `kv.put` refuses a
function, and that a response whose app set `content-length: 99` carries one
content-length, the body's (without the SDK fix it carried none);
`limits_kill_runaway_javascript` adds runaway recursion (a trap, the instance
replaced, storage intact). All pass. Also checked by hand:
top-level `Date.now()`, `Math.random()`, `crypto.randomUUID()`,
`console.log`, waiting on a timer, a syntax error and a missing
`export default { fetch }` each fail `js/build.sh` with the app's error.

## For the orchestrator

- **Docs:** the README's "JavaScript apps" text (if folded in from js.md)
  should say QuickJS and that `js/build.sh` needs `WASI_SDK_PATH`; notes/js.md's
  "Engine: Boa" paragraph is superseded by this file.
- **CI:** the WASI SDK install step moved before the JS build.
- **Dependencies of the pick:** the wave-1 JS commits (a26bfa7 c63b06f 9cde8c2
  95d3212 70f9516 050885b) and the SDK commit of this branch.
