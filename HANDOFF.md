# Handoff

Everything a new agent needs to pick this project up. Read this first, then [PLAN.md](PLAN.md) (design and phases) and [README.md](README.md) (what exists, results, bugs found).

## State (2026-10-28)

- **`main` (`227f8bd`) is current.** It has phase 1 (portable core), phase 2 (the Linux jail), and two simplification passes. Every branch is pushed; nothing is uncommitted.
- **Next: phase 3, signed OCI deploys.** Three open decisions are waiting on the owner (see [Open questions](#open-questions-for-phase-3)). Don't build phase 3 until they're answered.
- **Tests:** 41 integration tests pass (`cargo test --release -p nanovm`), plus 3 unit tests. One test is ignored: `egress_listed_host_allowed` makes a real HTTPS request to example.com, and this sandbox has no DNS or direct internet. The demo (`demo/demo.sh`) runs with 0 failures. `scripts/vm-test.sh` passes (cgroup tests and the jail probe, in a QEMU VM).
- **Runtime size:** 3,362 lines (`crates/nanovm/src`, excluding `#[cfg(test)]`). It was 2,303 before the jail.
- **Old branches** (`jail`, `simplify`, `loc-reduction`, `linux-baseline`, `quota-rename-self-repro`) are merged or historical; `main` contains everything.

## What nanovm is

A wasmtime-50, WASIp3 host that runs untrusted HTTP components as Durable-Objects-style objects:
- one live instance per `(app, object)`, serving one request at a time
- memory lost on eviction; a persistent `/data` volume per object
- hard limits on memory, CPU, deadlines, handles, disk and file count, plus an egress allowlist

`nanovm serve` is a supervisor that runs no guest code. It runs one jailed worker process per app (uid, Landlock, seccomp, optional cgroup v2) and makes every outbound request on the workers' behalf. The supervisor and workers talk over "links": HTTP/1.1 over socketpairs passed with `SCM_RIGHTS`.

## The agreed goal (confirmed by the owner)

One node, as fast and secure as we can make it, that runs untrusted Wasm apps as durable objects, deployable by anyone without handing out a long-lived secret. It is a solid single-node baseline before any clustering.

Decisions from the design interview (Q numbers as used in the conversation):

| # | Decision |
|---|---|
| Q1 | The node sits behind a trusted proxy on a private network; no TLS or auth on the node. The proxy may be public; only the proxy-to-node hop is private. |
| Q3 | Tenant = app: one jailed worker per app. |
| Q5 | Path routing `/{app}/{object}/{path}`. |
| Q6 | No cluster for now. |
| Q7 | Apps come from an OCI registry, pulled by digest only, with a content-addressed cache. |
| Q8/Q19 | Keyless Sigstore signing: GitHub attestations from CI, or `cosign` with browser OIDC on a laptop. Verify offline with the `sigstore-verify` crate against the embedded trusted root. As few long-lived secrets as possible, everywhere. |
| Q9/Q20 | Stateless: the proxy sends the manifest digest with every request; the node fetches on first use (single-flight per digest), verifies, compiles in the jail and caches. No deploy API, no deploy state. |
| Q10 | New requests go to the new version; old objects drain and are evicted. |
| Q11 | Manifest limits are clamped to operator maximums. |
| Q12 | Untrusted Wasm is compiled inside the app's jailed worker. |
| Q15 | Key-value is a host-implemented `wasi:keyvalue` stored as files in `/data`; `wasi:filesystem` stays exposed. |
| Q16 | WebSockets follow tric's model (`github.com/carsonfarmer/tric`, `src/ws.rs`): Pushpin WebSocket-over-HTTP events (OPEN/TEXT/CLOSE/DISCONNECT) delivered as normal bounded requests, plus GRIP-style channels. The node speaks it itself (no Pushpin process). Ordered per object, per-object channels and socket cap, tric's size bounds (128 KiB message, 32 KiB frame, 64 events per answer, 2 MiB answer). Sockets outlive instances. |
| Q17 | Alarms: one per object, set by the response header `x-nanovm-alarm: <unix-ms>` (0 clears), persisted, delivered at least once with backoff as a POST with a host-only marker stripped from client requests. |
| Q18 | Running agent-written one-shot code is not a goal for now. |
| Q21 | Trust policy per app: a list of exact `{issuer, subject}` identities; no static public keys. |
| Q22 | The trust policy and minimum version travel with each request from the proxy. |
| Q23/Q24 | The jail and the supervisor/worker split as built. Mount and network namespaces were dropped as redundant: Landlock already confines the filesystem, and seccomp removes `socket()`. |

Phases after this one ([PLAN.md](PLAN.md)): 3 signed OCI deploys, 4 key-value, 5 alarms, 6 WebSockets.

## Open questions for phase 3

These were asked and are unanswered. Each has a recommended answer; "yes" from the owner accepts it.

- **Q25. How to test signatures.** This sandbox reaches `ghcr.io` but not Sigstore's Fulcio, Rekor and TUF, or GitHub's OIDC endpoint, so nothing can be signed here.
  - Recommended: add a GitHub Actions workflow to `carsonfarmer/nanovm` that builds the guests, pushes them to `ghcr.io/carsonfarmer/nanovm-<guest>` as public packages, and attaches GitHub attestations. Tests then verify the real chain, and the negative cases come for free (wrong identity, tampered digest, missing attestation, version below the floor).
  - **This needs the owner's explicit OK**: it pushes a workflow and publishes public packages under their account.
  - The alternative is a fake Sigstore inside the tests. It's much more code and never checks the real chain.
- **Q26. Request protocol.** Recommended headers from the proxy:
  - `nanovm-app: ghcr.io/org/app@sha256:…`
  - `nanovm-trust: <issuer> <subject>`, comma-separated for several identities
  - `nanovm-min-version: <semver>`

  The version is the manifest's `org.opencontainers.image.version` annotation, covered by the signature. `/{app}/` in the path still names the app and its `/data`.
- **Q27. Local `[[app]]` entries.** Recommended: keep them as operator-pinned local apps for development, the demo and tests, alongside registry apps. Otherwise every test and the demo depend on a registry.

Research already done for phase 3:
- **Crates:** `oci-client` 0.18 (anonymous pull of public GHCR works; `ghcr.io` answers 401 until a token is fetched) and `sigstore-verify` 0.15.1 (sigstore-rust, Apache-2.0).
  - `Verifier::new(&TrustedRoot)` takes any trusted root, and `TrustedRoot::from_json(SIGSTORE_PRODUCTION_TRUSTED_ROOT)` is the embedded one.
  - `IdentityMatcher` does exact matching, so also check the certificate's repo and workflow claims.
  - Siblings: `sigstore-sign`, `sigstore-bundle`, `sigstore-trust-root` (all 0.15.1).
- **Artifact layout:**
  - `artifactType application/vnd.nanovm.app.v1+json`
  - a config blob with limits and env, and an `application/wasm` layer
  - the Sigstore bundle attached as a referrer
  - the signature is over the manifest digest
- **Where the work goes:** per Q24, the supervisor fetches and verifies, then passes bytes to the worker on stdin. The hook is in `Supervisor::start` ("this is where a fetched and verified artifact will come from"). Defend against downgrades with the per-app minimum version.

## How to work with this owner

- Ask **few questions at a time**, grilling-style: number them and give a recommended answer, so "yes" accepts it.
- **Never write backward-compatibility code.**
- Minimal runtime lines. The owner wants real logic review (unnecessary defense in depth, repeated logic), not cosmetic trims. Test lines don't count.
- Fix root causes, not band-aids. Build and run the tests and the demo before claiming anything works.
- Benchmarks must not regress. Every limit must be proven by a hostile test, and mutation-check new tests (they must fail without the fix).
- Never say you lack access to something: find a way (clone public repos, use the proxy, boot a VM). The Spin repos were cloned this way.
- **Git:**
  - Work on a branch, then fast-forward `main` when it's done and tested. The owner has OK'd merging to `main` without PRs; don't open PRs unless asked.
  - Commit messages end with the Co-Authored-By and Claude-Session lines from the system reminder.
  - Ask before outward-facing actions: new public packages, workflows, anything published.
- **Hosts:**
  - Never touch the `hetzner-*.fleet.recall.network` hosts.
  - Use `lost-rain.boxd` only after asking.
  - Don't SSH anywhere without asking.
- Use `model: "opus"` for subagents doing judgement work.
- Keep replies brief.

## Environment gotchas (this cloud sandbox)

- **`vm.max_map_count`** resets when the container restarts. Run `sysctl -qw vm.max_map_count=1048576` before tests or the demo; the worker's startup check names the fix if you forget.
- **The fd limit is 20,000** and can't be raised (no `CAP_SYS_RESOURCE`). The demo uses `max_live_objects = 8000` per app, and the full benchmark needs `--quick`.
- **Never run with `RUST_BACKTRACE` set** (`env -u RUST_BACKTRACE`). wasmtime captures a backtrace for every guest error, which costs about 3×.
- **cgroup v2 here has only hugetlb**; memory, cpu and pids belong to cgroup v1. Use `scripts/vm-test.sh` (QEMU with software emulation, an Ubuntu kernel) to test cgroups. CPU-budget and latency tests fail under emulation, so the default set avoids them.
- **No DNS and no direct internet**; outbound HTTPS goes through a proxy the egress code doesn't use. `ghcr.io` and github.com work through it, but sigstore.dev doesn't.
- **`pkill -f <pattern>` kills your own shell** if the pattern appears in the command line; use `pkill -x nanovm`.
- **Waiting on the demo:** grep for `node still running`, not `Error`, because the output contains `ErrorCode::…`.
- **Workers run as uid 200000+** when the supervisor is root. Anything they must reach (static dirs, data, the repo path) must be traversable by that uid; `/tmp/claude-0` (the scratchpad) is mode 700.
- **Profiling:** use `perf` at `/usr/lib/linux-tools-6.8.0-147/perf` (the default wrapper fails for this kernel).

## Layout pointers

- `crates/nanovm/src`: the runtime.
  - `supervisor.rs` (supervisor and worker entry), `link.rs`, `jail.rs`
  - `egress.rs` (`Route::Direct` in-process, `Route::Supervisor` jailed)
  - `object.rs`, `table.rs` (`Node`), `quota.rs`, `server.rs`, `config.rs`, `engine.rs`, `app.rs`, `metrics.rs`, `main.rs`
- `crates/nanovm/tests/integration.rs`: the integration tests. Helpers: `node_cfg`, `supervised_with`, `http_get`, `test_cgroup`, `stalling_upstream`. `jail_probe` runs in a child process.
- `crates/nanovm/support/load.rs`: the in-process load generator, used by the tests and the benchmark.
- `crates/nanovm/examples/bench.rs`: the in-process benchmark.
- `guests/`: `counter`, `hostile` and `util`; build them with `scripts/build-guests.sh`. Hostile endpoints include `/hold` (keeps response bodies open).
- `demo/demo.sh` and `demo/nanovm.toml`.
- `scripts/vm-test.sh`.
