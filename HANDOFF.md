# Handoff

Everything a new agent needs to pick this project up. Read this first, then [PLAN.md](PLAN.md) (design and phases) and [README.md](README.md) (what exists, results, bugs found). [ISSUES.md](ISSUES.md) lists known problems and who has them; check it before fixing anything.

## State (2026-10-30)

- **`main`** has:
  - phase 1 (portable core);
  - phase 2 (the Linux jail);
  - two simplification passes;
  - phase 3 (signed OCI deploys);
  - phase 4 (`wasi:keyvalue`, `wasi:config`);
  - phase 5 (WebSockets, after two reviews and a reduction pass);
  - 64 unit tests added by an agent, which found two bugs (now fixed).
  - phase 6 (per-request commits, Q37–Q40), after an adversarial review whose findings are fixed. README's "Commits" section describes the design.
  - phase 6b (calls between objects, Q45). README's "Calls between objects" describes it.
  - orb setup (`.agents/setup`, `.agents/resume`), so Amp orbs start with the toolchain and the tests prebuilt.
- **A second review** (at `c607ec9`) found N-1 to N-21, tracked in [ISSUES.md](ISSUES.md). Those in `journal.rs`, `quota.rs`, `kv.rs`, `jail.rs` and `table.rs` are fixed. Those in `supervisor.rs`, `registry.rs` and `config.rs` are deferred to the rewrite there, and so are N-13 to N-15, whose fix needs the supervisor's call site.
- **In progress elsewhere:** another agent is rewriting `supervisor.rs`, `registry.rs`, `config.rs` and the integration-test helpers for the front proxy (Q34): local apps removed, app data keyed by repository, `nanovm proxy` added. Its work in progress is on the remote branch `proxy`. Don't edit those files without coordinating; see "Working alongside other agents".
- **Next:** PLAN.md's "Roadmap" (agreed 2026-10-30), tracked as `W-1`… in ISSUES.md. In short: the proxy with WebSockets moved into it, OpenTelemetry by default (high priority), the per-object SQLite demo, alarms outside the node, data lifecycle planning, then the groundwork for many nodes.
- Branches merged into `main` by fast-forward are not always pushed on their own. The remote `config` branch is stale: it was merged by a fast-forward of a rebased copy.
- **Tests:** 56 integration tests and 84 unit tests pass. One is ignored: `egress_listed_host_allowed` makes a real HTTPS request to example.com, which this sandbox can't reach directly. The registry tests run against the real CI-signed guests on ghcr.io.
  - `cpu_hogs_leave_cores_for_short_requests` is load-sensitive and fails occasionally when the whole suite runs at once on 4 cores; it passes alone.
  - When guests change, the first run can fail one supervised test with `Permission denied`. The test nodes share one `.cwasm` cache between root in-process nodes and jailed workers. A rerun passes.
- **Runtime size:** about 5,250 lines (3,704 after phase 3; kv and config added about 300, WebSockets about 640, commits about 500 non-blank, non-comment lines, against an estimate of 250–300, which the owner accepted after a reduction pass).
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
| Q37 | A request's durable changes and its WebSocket publishes take effect only if it succeeds: an output gate, as in Durable Objects. |
| Q38/Q39 | Real files on `/data` stay (the owner rejected key-value-only storage). The mechanism is an undo journal under our `wasi:filesystem` host, crash-safe, with fsync before the response. Overlays, snapshots, FUSE and existing crates were surveyed and rejected (see README "Commits"). The owner's limit: stop and ask if it costs about 2× the estimate. It did, and the owner asked for a reduction pass. |
| Q40 | Success means the request answered, whatever the status. A guest error, trap, CPU or time limit, or crash rolls back and restarts the instance (the Durable Objects rule; tric's "not 5xx" rule was tried and dropped). The quota counts committed state, and the journal may hold as much again. `/tmp` and `/data` stay separate: `/tmp` is fast, non-durable scratch, and renames between them are cross-device. |
| Q18 | Running agent-written one-shot code is not a goal for now. |
| Q21 | Trust policy per app: a list of exact `{issuer, subject}` identities; no static public keys. |
| Q22 | The trust policy and minimum version travel with each request from the proxy. |
| Q23/Q24 | The jail and the supervisor/worker split as built. Mount and network namespaces were dropped as redundant: Landlock already confines the filesystem, and seccomp removes `socket()`. |
| Q41 | Alarms: queued again soon, built outside the core node as a scheduler beside the proxy. |
| Q42 | WebSockets move out of the node into the front proxy, Pushpin-style: the proxy holds client sockets, objects keep receiving events as plain requests. Supersedes Q31. |
| Q43 | Observability is high priority: OpenTelemetry tracing, metrics and guest logs on by default, per tenant. The custom `/_nanovm/stats` and `/_nanovm/events` APIs are deprecated. |
| Q44 | Data lifecycle (deleting, exporting, backing up an app's or object's data) needs a plan; queued. |
| Q45 | Calls between objects of one app: an outgoing request to `http://nanovm/objects/{id}/{path}`, through the callee's queue; cycles refused with 508 (done). |
| Q46 | The per-object SQLite demo is the priority showcase. |
| Q47 | JS and Python guests, `nanovm publish`, and benchmarks against workerd and Spin are tracked as issues, after the SQLite demo. |
| Q48 | Research shipping commit change records off the node (Litestream-style), so a node becomes a cache of its volumes. |
| Q49 | Prepare for many nodes across regions: placement in the proxy, leases with epochs checked at commit; nodes stay unaware of each other. After the proxy. |
| Q50 | Requests that don't write can run in parallel (stateless routes anywhere, read-only requests on replicas); research, since the in-place undo journal makes same-node readers see uncommitted data. |
| Q51 | Clean up the review's removable-code list: done where it touched files outside the rewrite; the rest is W-13. |

Phases ([PLAN.md](PLAN.md)): 3 signed OCI deploys, 4 key-value and config, 5 WebSockets, 6 per-request commits, 6b calls between objects (all done); then PLAN's "Roadmap", starting with the front proxy.

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
  - Commit messages end with the trailers your tool adds: Claude Code agents the Co-Authored-By and Claude-Session lines from the system reminder; Amp adds an `Amp-Thread-ID` trailer itself.
  - Ask before outward-facing actions: new public packages, workflows, anything published.
- **Hosts:**
  - Never touch the `hetzner-*.fleet.recall.network` hosts.
  - Use `lost-rain.boxd` only after asking.
  - Don't SSH anywhere without asking.
- Use `model: "opus"` for subagents doing judgement work.
- **Look for an existing library before designing** anything substantial, and say what you found. The owner expects it.
- **Design from the problem, not from the code we have.** Present the correct, minimal design, and say when it differs from what fits the code today.
- Keep replies brief.

## Working alongside other agents

Several agents work on this repo at once, each in its own checkout.
- **ISSUES.md is the shared list.** Check it before starting; mark an entry `fixing` or `in progress` (with who) and push before you begin, and `fixed in <commit>` / `done in <commit>` in the same push as the work. Record anything new you find there, even if you don't fix it.
- **Own files, not features.** Whoever is rewriting a file owns it until they land; ISSUES.md and the State section above say who has what. A finding in someone else's files is recorded with `deferred: <why>` and left for them.
- **Land small, on top of `main`.** One finding or a small related group per branch; rebase onto `origin/main`, run the full suite (`env -u RUST_BACKTRACE cargo test -p nanovm`), fast-forward `main`, push. Never rewrite pushed history.
- **Docs move with the code.** README (what exists, tests, bugs found), PLAN (design and roadmap), HANDOFF (state, decisions, how to work) and ISSUES are updated in the same push as the change they describe.

## Environment gotchas: Amp orbs

- **Setup is automatic.** `.agents/setup` installs stable Rust with the `wasm32-wasip2` target, fetches both workspaces' crates, builds the test guests and prebuilds the tests (about 8 minutes cold on 4 cores; a fresh orb usually starts from a snapshot where it's done). It leaves the release build to the first command that needs it (demo, bench, `scripts/vm-test.sh`).
- **cargo is on the PATH of login shells** (`~/.cargo/env` from `~/.bash_profile`). A shell started before setup ran may not have it: use `bash -lc '…'`.
- **`.agents/resume` raises `vm.max_map_count`** on every start and wake (sudo works without a password in orbs).
- **The soft fd limit is 1,024** (hard limit 1,048,576): run `ulimit -n 100000` before the demo or the benchmark.
- **The demo works in orbs** since N-21. Outbound HTTPS works directly (the egress test to example.com passes in the demo).
- **`strace` installs with `sudo apt-get install -y strace`**; it is how the seccomp allowlist was traced (`strace -f -e trace=<syscall>` over the integration tests).
- Everything below about the earlier cloud sandbox still holds unless an orb note above says otherwise; `RUST_BACKTRACE` isn't set in orbs, but keep using `env -u RUST_BACKTRACE`.

## Environment gotchas (the earlier cloud sandbox)

- **`vm.max_map_count`** resets when the container restarts. Run `sysctl -qw vm.max_map_count=1048576` before tests or the demo; the worker's startup check names the fix if you forget.
- **The fd limit is 20,000** and can't be raised (no `CAP_SYS_RESOURCE`). The demo uses `max_live_objects = 8000` per app, and the full benchmark needs `--quick`.
- **Never run with `RUST_BACKTRACE` set** (`env -u RUST_BACKTRACE`). wasmtime captures a backtrace for every guest error, which costs about 3×.
- **cgroup v2 here has only hugetlb**; memory, cpu and pids belong to cgroup v1. Use `scripts/vm-test.sh` (QEMU with software emulation, an Ubuntu kernel) to test cgroups. CPU-budget and latency tests fail under emulation, so the default set avoids them.
- **No DNS and no direct internet**; outbound HTTPS goes through a proxy the egress code doesn't use. `ghcr.io`, github.com and `pkg-containers.githubusercontent.com` (where ghcr.io redirects blob downloads; the owner allowed it) work through it, but sigstore.dev doesn't. The registry client (`oci-client` over reqwest) honours the proxy and the system CA store.
- **`pkill -f <pattern>` kills your own shell** if the pattern appears in the command line; use `pkill -x nanovm`.
- **Waiting on the demo:** grep for `node still running`, not `Error`, because the output contains `ErrorCode::…`.
- **Workers run as uid 200000+** when the supervisor is root. Anything they must reach (static dirs, data, the repo path) must be traversable by that uid; `/tmp/claude-0` (the scratchpad) is mode 700.
- **Profiling:** use `perf` at `/usr/lib/linux-tools-6.8.0-147/perf` (the default wrapper fails for this kernel).
- **`RUST_BACKTRACE=1` is set in this environment.** Always run tests with `env -u RUST_BACKTRACE`.
- **Test builds are optimized** (`[profile.dev.package]` in `Cargo.toml`): host work inside guest calls counts against the guest's CPU budget, and unoptimized wasmtime made kv tests fail. The first build after a clean checkout takes about 10 minutes.
- **Mutation checks:** restore with `git checkout -- <file>` and confirm `git diff` is empty afterwards. A container restart once interrupted a script, and its mutation was committed.
- **docs.rs is blocked** by the network policy. Read crate sources through cargo (`cargo fetch` in a scratch crate, then `~/.cargo/registry/src`), or ask the owner to allow `docs.rs`.
- **The container restarts without warning** (uptime resets). Commit often.

## Layout pointers

- [ISSUES.md](ISSUES.md): known problems and planned work, with status. [PLAN.md](PLAN.md): design and roadmap.
- `.agents/setup`, `.agents/resume`: orb setup.
- `crates/nanovm/src`: the runtime.
  - `supervisor.rs` (supervisor and worker entry), `registry.rs`, `link.rs`, `jail.rs`, `ws.rs` (WebSockets), `kv.rs` (`wasi:keyvalue`), `journal.rs` (per-request commits)
  - `egress.rs` (`Route::Direct` in-process, `Route::Supervisor` jailed; also calls between objects and held publishes)
  - `object.rs`, `table.rs` (`Node`), `quota.rs`, `server.rs`, `config.rs`, `engine.rs`, `app.rs`, `metrics.rs`, `main.rs`
- `crates/nanovm/tests/integration.rs`: the integration tests. Helpers: `node_cfg`, `supervised_with`, `http_get`, `test_cgroup`, `stalling_upstream`. `jail_probe` runs in a child process.
- `crates/nanovm/support/load.rs`: the in-process load generator, used by the tests and the benchmark.
- `crates/nanovm/examples/bench.rs`: the in-process benchmark.
- `guests/`: `counter`, `hostile` and `util`; build them with `scripts/build-guests.sh`.
  - Hostile endpoints include `/hold` (keeps response bodies open), `/big` (a large body) and `/commit` (writes, then ends as `then=` says).
  - The counter guest has a chat room on `/ws` and `POST /publish`.
- `demo/demo.sh` and `demo/nanovm.toml`.
- `scripts/vm-test.sh`.
