# nanovm-sdk

Write a [nanovm](../../README.md) app in Rust: one async function from an
`http::Request` to an `http::Response`, the [`http`](https://docs.rs/http)
crate's types (re-exported as `nanovm_sdk::http`). Each `/{object}/…` URL is
its own durable object, with its own files and key-value store, served one
request at a time.

## Five minutes

Once: Rust's WebAssembly target, `wac` (it composes the key-value store into
apps), and `nanovm` itself.

```sh
rustup target add wasm32-wasip2
cargo install --locked wac-cli@0.12.0
git clone https://github.com/carsonfarmer/nanovm
cargo install --locked --path nanovm/crates/nanovm
```

An app:

```sh
cargo new --lib hello && cd hello
cargo add --git https://github.com/carsonfarmer/nanovm nanovm-sdk
printf '\n[lib]\ncrate-type = ["cdylib"]\n' >> Cargo.toml
```

`src/lib.rs`:

```rust
use nanovm_sdk::{Body, Result, http, kv};

nanovm_sdk::serve!(handle);

async fn handle(_req: http::Request<Body>) -> Result {
    let visits = kv::increment("visits", 1).await?;
    Ok(http::Response::new(format!("{} has had {visits} visits\n", nanovm_sdk::id()).into()))
}
```

Build and run it, then ask it things from another terminal:

```sh
../nanovm/tools/build-app.sh --run   # cargo build, compose the key-value provider, nanovm run hello.wasm
curl http://127.0.0.1:8080/alice/    # alice has had 1 visits
curl http://127.0.0.1:8080/alice/    # alice has had 2 visits
curl http://127.0.0.1:8080/bob/      # bob has had 1 visits
```

`nanovm run` prints a line per request and what the app prints, keeps data in
`./nanovm-data`, and restarts when `hello.wasm` changes: run
`../nanovm/tools/build-app.sh` again after an edit. Limits, `env` and `config`
go in `./nanovm.json` (`--manifest` names another file). It listens on
loopback only: there is no registry, signature or jail.

**Why a build script.** nanovm nodes link only the filesystem, HTTP and
config into apps. The key-value store is a Wasm provider over the object's
files, composed into the app with `wac plug`, so what `nanovm run` serves is
exactly what a production node will run. An app that doesn't use `kv` is
left as cargo built it.

## Commits

A request's file and key-value changes, and its WebSocket publishes, land
together when it answers. `?` (or a panic) rolls them all back, so there is
no need for write-to-temp-then-rename.

## What's in it

- Requests and responses are `http`'s: `req.method()`, `req.uri().path()`
  (the path within the object: `/{object}/a` arrives as `/a`),
  `req.headers()` (every value of a repeated header, bytes that aren't UTF-8
  kept), `Response::builder().status(201).header(k, v).body(body)?`,
  `StatusCode`, `header::CONTENT_TYPE`, …. A builder error converts into a
  failed request with `?`. Set `content-type` yourself; the
  [examples](../../examples) keep two-line `json(status, body)` and
  `text(status, body)` helpers for it.
- `Body`, both ways. A request's is read in full (the node bounds it by
  `max_request_body_kb`): `body()` derefs to its bytes, with `text()` and
  `json()` (feature `json`). Make one with `.into()` from a `String`, `&str`,
  `Vec<u8>`, `&[u8]` or `()`, or `Body::empty()`. A byte body gets its
  `content-length`.
- `RequestExt` (`use nanovm_sdk::RequestExt`) adds what `http` leaves out:
  `req.query("q")` (percent-decoded), `req.query_or("n", 0)`,
  `req.cookie("c")`.
- `Body::stream(|mut body| async move { body.write(..).await; Ok(()) })`
  streams a body after the handler returns; the object serves nothing else
  meanwhile, for at most the app's `post_return_timeout_ms`. Returning an
  error from it breaks the body off, so the client sees it end early rather
  than a short body that looks whole, and rolls back what it changed, as a
  panic does. `sse(id, event, data)` formats a Server-Sent Event
  ([examples/events](../../examples/events)).
- `data()` and `tmp()`: `read` (an `Option`), `read_text`, `write`, `append`,
  `remove`, `rename`, `open`; `fs::read_at(&fd, offset, len)` and
  `fs::write_at` for pieces of a file.
- `kv::get/get_text/set/delete/exists/increment/keys` on the store
  `default`; `kv::open(label)` for others.
- `sqlite::query(sql, &[&a, &b])` (its `Rows`: `iter()`, `first()`,
  `row.get::<T>("column")`, `json()`), `sqlite::execute` (rows changed) and
  `last_insert_rowid`, on the object's own database. The request is the
  transaction: `BEGIN`/`COMMIT` are refused, and each statement that writes
  costs its own syncs, so write a batch in one statement (`json_each(?)`).
  Building it needs `WASI_SDK_PATH` (SQLite is C);
  [examples/leaderboard](../../examples/leaderboard) uses it.
- `config::get(key)`: the manifest's `config` table.
- `fetch(Request::post(url).header(..).body(..)?)`: outbound HTTP to hosts
  in the manifest's `egress` list, answered with an `http::Response<Body>`;
  `fetch_streaming(request)` instead returns once the headers arrive, its
  body read as it comes (`next()`: `Ok(Some(piece))`, then `Ok(None)` once
  all of it has come). A body that breaks off (the connection closes before
  its `content-length` or last chunk) is an error from `fetch`, and from
  `next()` where it ends, never a short body that looks whole.
- `object("lobby").post(path, body)`, `.get(path)`, `.fetch(request)`:
  calls another object of the app, through its queue; it commits before it
  answers.
- `ws::events(&req)`, `ws::Answer` (`accept`, `subscribe`, `text`, `binary`,
  `close`; `.into()` the response) and `ws::publish(channel, msg)` for
  WebSockets.
- `after(async { … })`: work that runs once the client has the whole answer
  (or has gone), before the object's next request; work it passes to
  `after` runs too. What it changes commits when all of it is done, and
  rolls back if any of it panics or runs out of time; the answer stands.
  The main README's "Commits" has the model, which `ctx.waitUntil` shares.
- `id()`, `app()`, `env(key)`, `sleep(d)`, `quote(s)`; `wasip3` for the rest.

The [chat example](../../examples/chat) uses most of it;
[examples/echo](../../examples/echo) answers with what it was sent, to see
what reaches an app.

## Publish

Production nodes run only signed apps from an OCI registry, routed by a DNS
TXT record:

```sh
nanovm publish hello.wasm ghcr.io/you/hello:1.0.0   # pushes it with ./nanovm.json; prints how to sign and route
```

It prints the TXT record to add and how to sign keylessly: the GitHub
workflow template [publish-app.yml](../../.github/workflows/publish-app.yml)
(an artifact attestation from CI), or `cosign` from a laptop. README "Apps"
has the details.
