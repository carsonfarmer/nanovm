# onramp: from nothing to a running, published app

Before this branch, an app's first response needed raw `wasip3` bindings, a
hand-run `wac plug` of the key-value provider, an OCI push (oras, or Rust
against oci-client), a dev-mode node with a config file, and `nanovm-app` /
`nanovm-trust` headers on every request; production also needed a signature
and a DNS record, with no tool that said how. Now:

| | before | after |
|---|---|---|
| app code for "count visits per object" | raw bindings; the counter guest (more routes) is 153 code lines on a 236-line helper crate | 6 code lines on `nanovm-sdk` (examples/hello) |
| build | `cargo build` + `wac plug app.wasm --plug <provider path>` | `tools/build-app.sh` (composes only if the app imports key-value) |
| run locally | dev registry, push, `nanovm.toml` with `dev = true`, a `--features dev` node, magic headers | `nanovm run hello.wasm` (or `build-app.sh --run`), plain `curl` |
| publish | oras with the right artifact/config/layer types and version annotation; nothing checked the component first | `nanovm publish hello.wasm ghcr.io/you/hello:1.0.0`, which checks what a node checks and prints the signing steps and TXT record |
| JavaScript | not on main | `tools/build-app.sh app.js`, same `run`/`publish` |

## The first five minutes (a real run)

Recorded on this 4-core machine at load average 26–29 from other agents,
following the README quickstart in a fresh directory next to a clone of this
branch (`cargo add` with `--branch cf/onramp` because the SDK is not on main
yet; `--listen` on a free port because other agents use 8080). `nanovm` is
this branch's debug build: the one-time `cargo install --path
crates/nanovm` was started, then stopped at 4.8 GB of free disk shared with
other agents, so its time is not measured here. `wac` 0.12 and the
`wasm32-wasip2` target were installed; cargo's registry cache was warm, but
the app's target directory and the clone's key-value provider were not.

```text
$ cargo new --lib hello
      Creating library `hello` package
  note: see more `Cargo.toml` keys and their definitions at https://doc.rust-lang.org/cargo/reference/manifest.html
  (0.3 s)
$ cargo add --git https://github.com/carsonfarmer/nanovm --branch cf/onramp nanovm-sdk
      Updating git repository `https://github.com/carsonfarmer/nanovm`
        Adding nanovm-sdk (git) to dependencies
               Features:
               - json
      Updating git repository `https://github.com/carsonfarmer/nanovm`
      Updating crates.io index
       Locking 44 packages to highest Rust 1.99.1 compatible versions
  (2.2 s)
$ printf '\n[lib]\ncrate-type = ["cdylib"]\n' >> Cargo.toml
  (0.0 s)
$ cp ../nanovm/examples/hello/src/lib.rs src/
  (0.0 s)
$ cat src/lib.rs
  use nanovm_sdk::{Request, Response, Result, kv};
  
  nanovm_sdk::serve!(handle);
  
  async fn handle(_req: Request) -> Result {
      let visits = kv::increment("visits", 1).await?;
      Ok(Response::text(format!("{} has had {visits} visits\n", nanovm_sdk::id())))
  }
  (0.0 s)
$ ../nanovm/tools/build-app.sh --run --listen 127.0.0.1:18732 &   # one command: build, compose, serve
      Finished `release` profile [optimized] target(s) in 1m 04s
  build-app: hello.wasm (414863 bytes)
  nanovm: hello on http://127.0.0.1:18732/<object>/ with data in nanovm-data
  (127.0 s to serving)
$ curl -s http://127.0.0.1:18732/alice/
  alice has had 1 visits
  (0.1 s)
$ curl -s http://127.0.0.1:18732/alice/
  alice has had 2 visits
  (0.1 s)
$ curl -s http://127.0.0.1:18732/bob/
  bob has had 1 visits
  (0.1 s)
first response 129.8 s after `cargo new`
$ sed -i 's/has had {visits} visits/has visited {visits} times/' src/lib.rs
  (0.0 s)
$ ../nanovm/tools/build-app.sh
     Compiling hello v0.1.0 (~/hello)
      Finished `release` profile [optimized] target(s) in 1.63s
  build-app: hello.wasm (414879 bytes)
  (1.8 s)
$ curl -s http://127.0.0.1:18732/alice/
  alice has visited 3 times
  (0.0 s)
$ # what nanovm run printed:
  nanovm: hello on http://127.0.0.1:18732/<object>/ with data in nanovm-data
  GET /alice/ → 200 in 30.9 ms, cold start
  GET /alice/ → 200 in 10.0 ms
  GET /bob/ → 200 in 6.5 ms, cold start
  nanovm: hello.wasm changed; restarting
  nanovm: hello on http://127.0.0.1:18732/<object>/ with data in nanovm-data
$ ../nanovm/target/debug/examples/dev-registry 127.0.0.1:15731 &   # a registry on loopback
$ nanovm publish hello.wasm 127.0.0.1:15731/you/hello:1.0.0
  127.0.0.1:15731/you/hello:1.0.0@sha256:851e62facf8eabfb97360f03791b8b9fc5359289acc64a72fe815424d43d9942
  nanovm: pushed 127.0.0.1:15731/you/hello:1.0.0 (sha256:851e62facf8eabfb97360f03791b8b9fc5359289acc64a72fe815424d43d9942). Nodes pull it anonymously (make the package public) and run it only once it is signed. Sign it keylessly, either:
  - from GitHub Actions: .github/workflows/publish-app.yml attests it (actions/attest-build-provenance), or
  - from a laptop: cosign attest --yes --new-bundle-format --type custom --predicate <(echo '{}') 127.0.0.1:15731/you/hello@sha256:851e62facf8eabfb97360f03791b8b9fc5359289acc64a72fe815424d43d9942
  Then route a host to it with a DNS TXT record naming the signer's OIDC issuer and identity:
  _nanovm.example.com. TXT "app=127.0.0.1:15731/you/hello:1.0.0; trust=<issuer> <identity>"
  such as trust=https://token.actions.githubusercontent.com https://github.com/<owner>/<repo>/.github/workflows/publish-app.yml@refs/heads/main
  (1.7 s)
```

- **First response: 130 s after `cargo new`**, of which the app's
  dependencies (wasip3, wit-bindgen) took 64 s and the first build of the
  key-value provider about 60 s (timed again alone: 60.4 s; it is built once
  per clone). The composition itself (`wac plug`) takes 12 ms. A second new
  app skips the provider; an edit-rebuild is **1.8 s**, and `nanovm run`
  restarts on it by itself (data kept: alice's count went on to 3).
- The quickstart's `hello.wasm` is 414,863 bytes with cargo's default
  release profile; `examples/hello` (opt-level `s`, LTO) is 331,380.
- Requests: a cold object 6.5–30.9 ms (the first includes the store's first
  write, a journaled commit with fsyncs), warm 10 ms, each a durable
  key-value write, on the debug build at load ~27.
- JavaScript, the same way (`tools/build-app.sh js/examples/counter.js` with
  the runtime already built: 23.7 s, 1,724,339 bytes): `POST /alice/` gave
  `{"object":"alice","n":1}` then `n:2`, `GET /bob/` `n:0`.

## Choices, and why

**Key-value is composed at build time, by `tools/build-app.sh`.** Nodes link
only the filesystem, HTTP and config; key-value is a Wasm provider the app
carries. The composed component is what `nanovm publish` pushes and what a
node runs, so composition belongs before both. Composing inside `nanovm run`
(the `wac-graph` library would make it about 20 lines) was rejected: `run`
would then serve an app that `publish` pushes uncomposed and every node
refuses, or `publish` would need the same machinery, and the binary would
carry a second wasmparser/encoder stack and the provider. The script is 40
code lines: `cargo build --release --target wasm32-wasip2`, then, only if
the component imports `spin:key-value` (an app that never calls `kv`
imports nothing: checked with wasm-tools), `wac plug` of the repository's
provider, built on first use. `nanovm run` and `nanovm publish` both refuse
an uncomposed component with a pointer to the script. The one extra install
is `wac` (W-101 proposes dropping it).

**The SDK is `sdk/rust`, not `guests/sdk`.** Anything under `guests/**`
republishes the signed test guests from main. The SDK is its own workspace,
so apps depend on it by git; its bindings read the WIT from
`components/keyvalue/wit` and `wit/config` in the same checkout (crates.io
would need them vendored, W-100). It is the earlier SDK with key-value
rewritten for the provider: Spin's interface is async, so `kv::get/set/...`
are `async`; the default store is opened once per instance (a store is a
label, not a descriptor, so this is safe across requests and rollbacks);
`increment` is a get and a set, atomic because an object serves one request
at a time and the request commits as a whole.

**`nanovm run` is the node's own request path plus what the proxy adds.**
An in-process `Node` serves `/{object}/…` through `server::handle`; WebSocket
upgrades go to the proxy's `ws::Sockets`; client-sent socket events are
refused as the proxy refuses them; a loopback OTLP sink prints a line per
request span and each guest log record. It re-executes itself when the
`.wasm` settles after a change: a request in flight ends as in a crash and
rolls back, and an object is never live twice on one volume. It refuses a
non-loopback `--listen`, needs no `dev` feature, and gives egress no
private-address exemption. `serve` is unchanged: registry-only, signed,
jailed. The one runtime change it needed: in-process nodes now deliver
publishes (N-200) instead of sending them to egress, which denied them.

**`nanovm publish` pushes, checks, and tells; it does not sign.** It pushes
the layout `publish-guests.yml` makes with oras (artifact type
`application/vnd.nanovm.app.v1+json`, `nanovm.json` as the config blob, the
component as the one `application/wasm` layer, the semver tag as
`org.opencontainers.image.version`), using `oci-client` (already a test
dependency; the node's own client only pulled). Since `cf/oci-push` it
pushes through that client, `registry::Oci::push_app`, and oci-client is
gone (the owner's call). Before pushing it
compiles the component and runs the node's own import check and handler
link, so an app a node would refuse never reaches the registry. Keyless
signing needs an OIDC identity: from CI that is GitHub's artifact
attestation (`.github/workflows/publish-app.yml`, a manual-only template);
from a laptop, cosign with a browser sign-in. The CLI prints both and the
`_nanovm` TXT record. The cosign line is not tested end to end (W-102).

**JavaScript keeps its API; `env.kv` became promise-based underneath.**
The QuickJS runtime's host functions for key-value now return promises
settled from the runtime's existing task queue, as fetch and timers already
were. Pre-initialization is unchanged: every import traps while the snapshot
is taken, and the store is opened at first use at run time.

## Runtime cost

CI's count of `crates/nanovm/src` (nonblank, noncomment, before test
modules): 5,184 on `cf/main-next` -> 5,355 on this branch (+171).

| | lines |
|---|---:|
| in-process publish delivery (`egress.rs` +1, `object.rs` +6, `table.rs` +2, `ws.rs` +11, `proxy.rs` −8) | +12 |
| `nanovm run` (`main.rs`: arguments, WebSocket front, console printer, restart on change) | +106 |
| `nanovm publish` (`main.rs`: checks, push, guidance; the shared key-value hint and the default `./nanovm.json` for `run`) | +53 |

Outside the runtime: `sdk/rust` 521 code lines, `tools/build-app.sh` 40,
`js/` runtime 414 Rust + 362 JavaScript, `js-wizer` example 68,
`examples/hello` 6, `examples/chat` 98 Rust + a page.

## Tests

All on this branch, `--locked`, `RUST_TEST_THREADS=1`, on 4 cores shared with
other agents.

- `tests/dev.rs` (new, drives the real binary): `nanovm_run_listens_on_loopback_only`,
  `nanovm_run_says_how_to_compose_key_value`, `nanovm_run_serves_requests_sockets_and_publishes`,
  `nanovm_run_restarts_when_the_component_changes`, `the_quickstart_app_counts_visits`,
  `the_chat_example_is_live_and_durable`, `javascript_apps_run_and_publish`,
  `nanovm_publish_pushes_what_a_node_loads`.
- `tests/js.rs` (ported): nine tests on an in-process node.
- `integration.rs`: `only_well_formed_publishes_are_held` (new).
- Mutation checks (change made, test seen failing, file restored):
  in-process publish not delivered -> the run socket test times out on "hi";
  egress still routing in-process publishes -> `only_well_formed_publishes_are_held`
  gets `{"denied":…}` instead of 404; `run` without the event refusal -> a forged
  event gets 200; `kv::increment` ignoring the stored value -> the quickstart test
  sees 1 visit, not 2; the chat lobby without its `.`/`_` rule -> `_x` accepted;
  `kv.put` not awaiting the host promise -> 6 of 9 JS tests fail; publish without
  the artifact type -> the node's loader refuses what was pushed; publish without
  the import check -> the uncomposed component fails without the hint.
- Full suite, twice, at load average 13–29 from other agents: unit 103/103,
  `dev` 8/8, `js` 9/9, `committed_state` (since removed) 2/2; integration 70 passed, 12 ignored
  and one timing failure each run, a different one each time
  (`kv_counts_against_the_disk_quota`, then `cpu_hogs_leave_cores_for_short_requests`),
  each of which passed in the other run. Both fail the same way with
  `cf/main-next`'s own test binary at that load (4/4 and 3/3 alternating
  base/new runs: `cpu` or `timeout` where the test expects a quota or `cpu`
  verdict), so they measure the machine, not this branch (N-203).
  `cargo check --locked -p nanovm --bin nanovm` (production, no `dev`
  feature) passes.

## Reproduce

```sh
export PATH=/path/to/wac-0.12/bin:$PATH WASI_SDK_PATH=/path/to/wasi-sdk-34.0-x86_64-linux
bash scripts/build-guests.sh
tools/build-app.sh --locked examples/hello -o examples/target/hello.wasm
tools/build-app.sh --locked examples/chat -o examples/target/chat.wasm
tools/build-app.sh --locked js/tests/app.js -o js/target/test-app.wasm
ulimit -n 20000; env -u RUST_BACKTRACE RUST_TEST_THREADS=1 cargo test --locked -p nanovm --test dev --test js
```

## Open

W-100 (SDK on crates.io), W-101 (compose without the wac CLI), W-102 (test
the cosign path), W-103 (warn when a package can't be pulled anonymously),
W-104 (test guests onto the SDK, needs the owner). `nanovm run` restarts on
a changed `.wasm`, not on a changed source: rerun `tools/build-app.sh`.
