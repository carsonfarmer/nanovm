# nanovm-sdk

Write a [nanovm](../../README.md) app in Rust: one async function from a
request to a response. Each `/{object}/…` URL is its own durable object, with
its own files and key-value store, served one request at a time.

```sh
rustup target add wasm32-wasip2
cargo install --locked --git https://github.com/carsonfarmer/nanovm nanovm
cargo new --lib hello && cd hello
cargo add --git https://github.com/carsonfarmer/nanovm nanovm-sdk
printf '\n[lib]\ncrate-type = ["cdylib"]\n' >> Cargo.toml
```

`src/lib.rs`:

```rust
use nanovm_sdk::{Request, Response, Result, data};

nanovm_sdk::serve!(handle);

async fn handle(_req: Request) -> Result {
    let visits: u64 = data().read_text("visits").await?.map_or(0, |n| n.parse().unwrap_or(0)) + 1;
    data().write("visits", visits.to_string()).await?;
    Ok(Response::text(format!("{} has had {visits} visits\n", nanovm_sdk::id())))
}
```

```sh
cargo build --release --target wasm32-wasip2
nanovm run target/wasm32-wasip2/release/hello.wasm &
curl http://127.0.0.1:8080/alice/   # alice has had 1 visits
curl http://127.0.0.1:8080/alice/   # alice has had 2 visits
curl http://127.0.0.1:8080/bob/     # bob has had 1 visits
```

`nanovm run` prints a line per request and what the app prints, keeps data in
`./nanovm-data`, and restarts when you rebuild. Limits, `env` and `config` go in
a `nanovm.json`: `nanovm run app.wasm --manifest nanovm.json`.

**Commits.** A request's file and key-value changes, and its WebSocket
publishes, land together when it answers. `?` (or a panic) rolls them all back.

**More:** `req.query("q")`, `req.header("h")`, `req.json()` (feature `json`);
`Response::json(..).status(201)`; `tmp()`; `kv::get/set/increment`;
`config::get`; `fetch(url).send()` (hosts in the manifest's `egress`);
`object("lobby").post(path, body).send()` to call another object;
`ws::events`, `ws::Answer` and `ws::publish` for WebSockets. The
[chat example](../../examples/chat) uses all of them.

**Deploy.** Production nodes run only signed apps from an OCI registry. Push
the `.wasm` with its `nanovm.json` as the config (`oras push`, artifact type
`application/vnd.nanovm.app.v1+json`), sign it without keys (a GitHub artifact
attestation from CI, or `cosign` from a laptop), and publish a TXT record:
`_nanovm.example.com  "app=ghcr.io/you/hello:prod; trust=<issuer> <subject>"`.
[publish-guests.yml](../../.github/workflows/publish-guests.yml) is a working
pipeline; README "Apps" and "The proxy" have the details.
