# The Rust SDK on the `http` crate's types (B-701)

Branch `sdk/http-types`, on `cf/round3`. The owner's decision: `nanovm-sdk`
drops its own `Request` and `Response` for the `http` crate's
(`http::Request`, `http::Response`, `StatusCode`, `HeaderMap`, …), for a
familiar API and a smaller SDK. What is nanovm's stays: `serve!`, `id`,
`app`, `env`, `kv`, `sqlite`, `fs`, `ws`, `after`, outgoing fetch and object
calls, and commits (an `Err` or a panic rolls the request's writes back).
No runtime code changed (`crates/nanovm/src`: +0 lines).

## Library first

- **`wasip3` 0.9's `http-compat` feature** (the bindings we already use, from
  the Bytecode Alliance) converts both ways: `http_from_wasi_request`,
  `http_from_wasi_response`, `http_into_wasi_*`, `HeaderMap` ↔ `Fields`,
  `Method`, `Scheme`, and a streaming `http_body::Body` for incoming bodies.
  Chosen: the SDK uses its conversions for every header map, method and
  scheme, both ways.
- **spin-sdk** 6–8 is built on exactly this feature (8.0 on `wasip3` 0.10):
  handlers take `http::Request<IncomingRequestBody>` and return
  `impl IntoResponse`, with `http-body-util` and `futures` for bodies.
- **wstd** (0.6.8) is WASI 0.2 only (`wasip2`), so it doesn't fit.

Not used from `http-compat`, and why:
- `http_from_wasi_request`/`_response`: they wrap the body in their streaming
  `IncomingBody`, and the common case wants the bytes. Taking the message
  back out of it (`take_unstarted`) works, but measured: building the parts
  from `Method`, `Scheme` and `HeaderMap`'s conversions directly (a URI
  builder, about ten lines) makes hello 11.8 KB and chat 8.7 KB smaller and
  skips four host calls a request (options, a second `headers`, its
  `content-length`, a drop). CPU per request couldn't tell the two apart on
  this machine (below).
- `http_into_wasi_response`/`_request`: they need an `http_body::Body` (so
  `http-body-util` for a plain byte body), never set `content-length` (so
  every answer would be chunked), always send a body (a `GET` would carry an
  empty chunked one), and a `Body::stream` writer can't be polled as one. The
  SDK's own body step is about ten lines.

## The API

| | before | after |
|---|---|---|
| handler | `async fn(Request) -> Result` | `async fn(http::Request<Body>) -> Result` |
| route | `match (req.method(), req.path())` | `match (req.method().as_str(), req.uri().path())` |
| body | `req.text()`, `req.json()`, `req.body()` | `req.body().text()`, `.json()`, `&req.body()[..]` (derefs to bytes) |
| query, cookie | `req.query("q")` (`&str`), `req.cookie("c")` | `req.query("q")` (`String`), `req.cookie("c")`, from `RequestExt` |
| header | `req.header("h")` | `req.headers().get("h")` (bytes; `.to_str()` for text) |
| answer | `Response::json(s).status(201).header(k, v)` | `Response::builder().status(201).header(CONTENT_TYPE, …).header(k, v).body(s.into())?` |
| stream | `Response::stream(type, \|b\| async { … })` | `Body::stream(\|b\| async { …; Ok(()) })`, an `Err` breaks the body off |
| fetch | `fetch(url).method("POST").header(..).body(..).send()` → `Reply { status, headers, body }` | `fetch(Request::post(url).header(..).body(..)?)` → `http::Response<Body>` |
| streamed fetch | `.stream()` → `Streaming { status, headers, next() }` | `fetch_streaming(req)` → `http::Response<Streaming>` (`next()`) |
| object call | `object(id).post(path, b).send()` | `object(id).post(path, b)`, `.get(path)`, `.fetch(req)` |
| WebSocket | `ws::events(&req)`, `answer.into()` | the same, on `http::Request<Body>` / `http::Response<Body>` |

`Body` is one type both ways: bytes (a request's, read in full; any answer's,
which gets its `content-length`) or a response's stream. `RequestExt` holds
the two things `http` leaves out: query parameters (decoded by
`form_urlencoded`, which replaces our own percent decoder) and cookies.
`nanovm_sdk::http` re-exports the crate, so apps need no version of their
own. The old types are gone, with no aliases (AGENTS.md rule 1).

Hello, before (round 3) and after, both 7 lines:

```rust
use nanovm_sdk::{Request, Response, Result, kv};

nanovm_sdk::serve!(handle);

async fn handle(_req: Request) -> Result {
    let visits = kv::increment("visits", 1).await?;
    Ok(Response::text(format!("{} has had {visits} visits\n", nanovm_sdk::id())))
}
```

```rust
use nanovm_sdk::{Body, Result, http, kv};

nanovm_sdk::serve!(handle);

async fn handle(_req: http::Request<Body>) -> Result {
    let visits = kv::increment("visits", 1).await?;
    Ok(http::Response::new(format!("{} has had {visits} visits\n", nanovm_sdk::id()).into()))
}
```

The new hello's answer has no `content-type` (`Response::text` set
`text/plain`); `curl` doesn't care, and the builder adds one.

**Round 3's fixes, carried over.** The branch merges `cf/round3` at 7b3f9c6,
whose r3-dev fixes changed the old SDK (N-710, N-713), and keeps them on the
new types. `after` work starts only once the answer is out (the future
`wit::Response::new` returns for its transmission), and `after` called
from it runs at once, before the next request. A `Body::stream` writer
returns `Result<()>`: an `Err` resolves the body's trailers to an error, so
the client sees the body cut off rather than a short one that looks whole,
and rolls back what was changed since answering, as a panic does. A
streamed request body (`fetch`) that fails only fails that request.
examples/gateway passes an upstream's break on to its client this way.

## Cost

All against `cf/round3` at 7b3f9c6, which this branch merges.

**Code.** The SDK's code lines (non-blank, non-comment, before tests): 830 →
776. `lib.rs` 322 → 286, the outbound module 107 → 84, `ws.rs` 83 → 88.
The examples grew: 2,909 → 3,208 lines for the 24 that existed (+299, +10%),
most of it 46 copies of two-line `json(status, body)` / `text(status, body)`
helpers that set `content-type`, which `Response::json` and friends did.

**Size.** Composed components as `tools/build-app.sh` builds them (release,
`opt-level = "s"`, LTO, stripped), both built at paths of the same length
(panic messages carry source paths):

| app | 7b3f9c6 | this branch | |
|---|---:|---:|---:|
| hello | 340,219 | 383,330 | +43,111 (+12.7%) |
| chat | 481,608 | 542,530 | +60,922 (+12.6%) |
| the other small apps | 396–515 KB | | +48–69 KB (+10.6–15.1%) |
| apps with SQLite | 1.53–1.81 MB | | +51–59 KB (+2.9–3.4%) |

That is noticeable. A linker map of hello (`-C link-arg=--Map`, unstripped,
on the first version of this branch, against round 3 at 9784334) puts it in
the `http` crate itself: code 110,174 → 158,021 bytes, data 15,852 → 21,782.
`HeaderName::from_bytes` alone is 16.9 KB (its table of standard header
names), `HeaderName::as_str` 2 KB, URI and `HeaderMap` code inlined into the
handler export +13 KB, `bytes` about 4 KB; our own SDK code shrank by
1.8 KB. Building requests directly instead of through
`http_from_wasi_request` (above) took 11.8 KB of that back. Any app on
`http` types pays the rest; the only way around it is not to use the crate.

**CPU per request.** CPU_PLACEHOLDER

## Tests

- SDK, natively (`cargo test --manifest-path examples/Cargo.toml -p
  nanovm-sdk`): query decoding (UTF-8, `+`, the last of several, a stray
  `%`, bytes that aren't UTF-8), cookies (several headers, one that isn't
  text), bodies, JSON bodies, a builder error failing the request, a
  socket's events from an `http::Request` and an `Answer` as an
  `http::Response`. Doc tests compile; `cargo doc` builds.
- `examples/echo` and `echo_maps_requests_and_responses_both_ways`: the
  mapping both ways through a real node, directly and through a call between
  objects (all four conversions): the status the app picks (200 to 599, an
  app's 500 not a failure), every value of a repeated header in order, header
  bytes that aren't UTF-8, the method, the URI within the object, the body's
  bytes and its `content-length`.
- Round 3's tests of the same behaviour pass on the new types:
  `review2_gateway_tells_its_client_when_the_upstream_breaks_off`
  (examples.rs), the `wait_until_*` tests (js.rs), and in integration.rs
  `after_work_*` and `a_streamed_body_whose_writer_fails_breaks_off_and_rolls_back`,
  on `crates/nanovm/tests/sdk-after`, ported to `http::Request<Body>`.
- Mutation checks, each restored with `git checkout -- sdk/rust/src/lib.rs`
  and an empty `git diff`, the app rebuilt both times:

  | break | caught by |
  |---|---|
  | answer every status as `200` | echo: `left: 200, right: 418` |
  | keep one value per header name (`insert`, not `append`) | echo: `["2"]`, not `["1", "2"]` |
  | header values made UTF-8, lossily | echo: `caf\u{fffd} …`, not `caf\xe9 \xff\x80` |
  | no `content-length` for bytes | echo: no `content-length` |
  | a stream writer's error ignored | gateway: the client's stream ends cleanly |
  | `query` keeps the first of several | SDK unit test: `7`, not `8` |
- End-to-end, serially (`RUST_TEST_THREADS=1`): `examples.rs` 26 passed
  (echo's new one included), `dev.rs` 9 (the quickstart's hello and
  chat among them), `js.rs` 11.

## JavaScript

JavaScript apps are unchanged: the JS runtime (`js/runtime`) is a Rust app on
the SDK, so it changed (it builds its answer with the `http` builder and
reads headers from a `HeaderMap`); the apps' `fetch(request, env, ctx)` and
`js/tests/app.js` didn't. A header value that isn't UTF-8 reaches JavaScript
with those bytes replaced, as before.

## Not done

- Response helpers (`json`, `text`, `html` with their content types) are
  not in the SDK; the examples keep their own. Owner question.
- `fetch` of a request that carries a `host` header (copied from an incoming
  request) fails: `wasi:http` forbids it. `http-compat` moves it into the
  URI's authority; the SDK doesn't (B-703).
