# Quality notes (quality agent, round 3)

Test and verification work from IDEAS.md sections D and G. Each item is on its own branch from
`cf/main-next`; this file collects the notes. Commands run from the repository root.

## D-52: deploy-time validation (`ideas/deploy-validation`)

**What it proves.** A component that imports something nanovm refuses is rejected when it is loaded,
before any object exists, and the error names the import:

| Fixture (`crates/nanovm/tests/refused/examples`) | Imports | Error names |
| --- | --- | --- |
| `p2_sockets` | `wasi:sockets/tcp-create-socket@0.2.12` (via `wasip2`) | ``import `wasi:sockets/…` is not permitted`` |
| `p3_sockets` | `wasi:sockets/types@0.3.0` | ``import `wasi:sockets/types@0.3.0` is not permitted`` |
| `p2_filesystem` | `wasi:filesystem/types@0.2.12` (from `std::fs`) | ``import `wasi:filesystem/types@0.2.12` …`` |
| `custom_import` | `acme:backdoor/shell` | ``import `acme:backdoor/shell` …`` |
| `command` | allowed imports only, exports `wasi:cli/run` | `wasi:http/handler` (missing export) |

The library in the same crate is the control: the same service with only allowed imports loads and
answers 200. Through a registry and a jailed worker, a request to the `p3_sockets` app is answered
502 `worker-down` with the import named in the body, and so is the retry.

**Why it matters (finding N-500).** `app::linker` adds all of WASIp2 (`p2::add_to_linker_async`),
and the instance's `WasiCtx` preopens `/data` and `/tmp` read-write. `p2_sockets` and
`p2_filesystem` both *link* against that linker (the test asserts `instantiate_pre` succeeds). For
sockets, wasmtime's default address check still denies every address, so there are two layers. For
files there is one: without `ALLOWED_IMPORTS`, `std::fs` could write `/data` outside the quota and
outside the journal (no rollback). Mutation check: with the allowlist test turned off
(`if false && …` in `check_imports`), both new tests fail (`p2_sockets was loaded`).

**Std pulls in more than you ask for.** The first try used `std::net::TcpStream` for the WASIp2
socket fixture; wasi-libc's socket code also imports `wasi:filesystem/types@0.2`, which came first,
so the error named the filesystem. The fixture now calls `wasip2` bindings directly. `check_imports`
reports only the first refused import; that is enough to reject, but an app author fixing them one
at a time would have to redeploy once per import.

Run: `cargo test --locked -p nanovm --test integration refuse` (builds the fixtures into
`target/test-guest` on first use; ~25 s cold).
