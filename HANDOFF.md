# Handoff

Everything a new agent needs to pick this project up. Read this first, then [PLAN.md](PLAN.md) (design and phases) and [README.md](README.md) (what exists, results, bugs found).

## State (2026-10-29)

- **`main` is current.** It has:
  - phase 1 (portable core);
  - phase 2 (the Linux jail);
  - two simplification passes;
  - phase 3 (signed OCI deploys);
  - phase 4 (`wasi:keyvalue`, `wasi:config`);
  - phase 5 (WebSockets).

  Every branch is pushed; nothing is uncommitted. The remote `config` branch is stale: it was merged by a fast-forward of a rebased copy.
- **Next: the front proxy** (Q34), once the owner confirms. Alarms are skipped for now (Q30).
- **Tests:** 51 integration tests pass, plus 5 unit tests. One is ignored: `egress_listed_host_allowed` makes a real HTTPS request to example.com, which this sandbox can't reach directly. The registry tests run against the real CI-signed guests on ghcr.io.
  - `cpu_hogs_leave_cores_for_short_requests` is load-sensitive and fails occasionally when the whole suite runs at once on 4 cores; it passes alone.
  - When guests change, the first run can fail one supervised test with `Permission denied`. The test nodes share one `.cwasm` cache between root in-process nodes and jailed workers. A rerun passes.
- **Runtime size:** about 4,670 lines (3,704 after phase 3; kv and config added about 300, WebSockets about 670 with the review fixes).
- **The repo `carsonfarmer/nanovm` is now public**, so GitHub artifact attestations work. The packages `ghcr.io/carsonfarmer/nanovm-{counter,hostile}` are public. `publish-guests.yml` runs on pushes to `guests/**` and on `gh workflow run publish-guests.yml` (dispatch works through the proxy, and `gh run watch <id>` shows progress). Each run publishes version `1.0.<run number>`.
- **Old branches** (`jail`, `simplify`, `oci`, `loc-reduction`, `linux-baseline`, `quota-rename-self-repro`) are merged or historical.

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
| Q17/Q30 | Alarms are skipped for now. Being header-based, they could be done outside the node. |
| Q31–Q36 | WebSockets live in the supervisor; Pushpin can't be embedded. Fan-out is to the object's own sockets only (Q32). Wire format as Q16. The front proxy is a later phase (Q34). The node stays stateless: sockets are in-memory and close with their worker, including on a version switch (Q35). Limits: `max_sockets` per object (256) and per node (10,000), plus the existing request and response limits; tric's API Gateway sizes weren't carried over (Q36). |
| Q18 | Running agent-written one-shot code is not a goal for now. |
| Q21 | Trust policy per app: a list of exact `{issuer, subject}` identities; no static public keys. |
| Q22 | The trust policy and minimum version travel with each request from the proxy. |
| Q23/Q24 | The jail and the supervisor/worker split as built. Mount and network namespaces were dropped as redundant: Landlock already confines the filesystem, and seccomp removes `socket()`. |

Phases ([PLAN.md](PLAN.md)): 3 signed OCI deploys, 4 key-value and config, 5 WebSockets (all done), 6 the front proxy.

## Phase 3 as built

The owner answered Q25–Q27 "yes", and chose to make the repo public rather than sign with cosign through public Sigstore from a private repo. The README's "Apps from a registry" and PLAN's "Phase 3" describe the result. Its pieces:
- `registry.rs` pulls by digest and verifies with `sigstore-verify` against the embedded production trusted root, checking the version floor *before* any blob is fetched. It caches by digest under `data_dir/registry`.
- The supervisor keeps a dynamic worker map, switches versions with a drain (one worker per app at a time), and gives each app the uid that owns its directory.
- The tests' signer identity is `https://token.actions.githubusercontent.com` with subject `https://github.com/carsonfarmer/nanovm/.github/workflows/publish-guests.yml@refs/heads/main`.
- The tests were mutation-checked: skipping verification, ignoring the floor, or not draining each makes a registry test fail.

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
- **No DNS and no direct internet**; outbound HTTPS goes through a proxy the egress code doesn't use. `ghcr.io`, github.com and `pkg-containers.githubusercontent.com` (where ghcr.io redirects blob downloads; the owner allowed it) work through it, but sigstore.dev doesn't. The registry client (`oci-client` over reqwest) honours the proxy and the system CA store.
- **`pkill -f <pattern>` kills your own shell** if the pattern appears in the command line; use `pkill -x nanovm`.
- **Waiting on the demo:** grep for `node still running`, not `Error`, because the output contains `ErrorCode::…`.
- **Workers run as uid 200000+** when the supervisor is root. Anything they must reach (static dirs, data, the repo path) must be traversable by that uid; `/tmp/claude-0` (the scratchpad) is mode 700.
- **Profiling:** use `perf` at `/usr/lib/linux-tools-6.8.0-147/perf` (the default wrapper fails for this kernel).

## Layout pointers

- `crates/nanovm/src`: the runtime.
  - `supervisor.rs` (supervisor and worker entry), `registry.rs`, `link.rs`, `jail.rs`, `ws.rs` (WebSockets), `kv.rs` (`wasi:keyvalue`)
  - `egress.rs` (`Route::Direct` in-process, `Route::Supervisor` jailed)
  - `object.rs`, `table.rs` (`Node`), `quota.rs`, `server.rs`, `config.rs`, `engine.rs`, `app.rs`, `metrics.rs`, `main.rs`
- `crates/nanovm/tests/integration.rs`: the integration tests. Helpers: `node_cfg`, `supervised_with`, `http_get`, `test_cgroup`, `stalling_upstream`. `jail_probe` runs in a child process.
- `crates/nanovm/support/load.rs`: the in-process load generator, used by the tests and the benchmark.
- `crates/nanovm/examples/bench.rs`: the in-process benchmark.
- `guests/`: `counter`, `hostile` and `util`; build them with `scripts/build-guests.sh`. Hostile endpoints include `/hold` (keeps response bodies open).
- `demo/demo.sh` and `demo/nanovm.toml`.
- `scripts/vm-test.sh`.
