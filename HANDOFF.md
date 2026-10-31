# Handoff

Everything a new agent needs to pick this project up. Read this first, then [PLAN.md](PLAN.md) (design and phases) and [README.md](README.md) (what exists, results, bugs found). [ISSUES.md](ISSUES.md) lists known problems and who has them; check it before fixing anything.

## State (2026-10-31)

- **CI enabled (W-16):** `ci-checks` passed GitHub run 41227208960 at `57d1595`: production check, 93 unit and 64 integration tests, one ignored. The initial run exposed a runner fd-limit mismatch; CI now uses its hard limit. Read-only permissions, no guest publishing. Branch protection was not changed.
- **Current work:** W-2 has started on `w2-observability`, incorporating main's CI. It owns telemetry, object/table instrumentation, server/supervisor and demo consumers. Its transport plan and readiness prerequisite are committed; no OTLP export exists yet. Guests remain unchanged.
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
  - phase 7 (the front proxy, Q34, Q52): `nanovm proxy`, apps only from registries and one app per worker, apps keyed by record owner and repository, dev mode, a registry client with size caps. Built by one agent on branch `proxy`, finished (rebase, demo, docs, reduction pass, review) by another. PLAN's "Phase 7" describes it.
  - orb setup (`.agents/setup`, `.agents/resume`), so Amp orbs start with the toolchain and the tests prebuilt.
- **A second review** (at `c607ec9`) found N-1 to N-21, tracked in [ISSUES.md](ISSUES.md). All are fixed, including N-20 with W-3. A review of phase 7 found N-22 to N-31; all are fixed (N-31 in part: its proxy TLS half is now W-14). N-32 (occasional test failures under load) is partly fixed.
- **Owner decisions:**
  - Nothing is waiting on the owner as of 2026-10-30. The owner decided: dev mode needs the `dev` cargo feature (N-23, done); drop TLS from the proxy (W-14, done); key apps by the DNS name that owns their `_nanovm` record, after CNAMEs, so aliases share one app (W-15, done).
- **W-3 complete:** migration `e62e9ac`, cleanup `bb8a418`, on branch `w3-proxy-sockets` for fast-forward into main with this validation record. Proxy sockets survive worker exits and deployments (Q53–Q56); no guest changes. Runtime: 4,247 baseline → 4,264 implementation → 4,246 cleanup (nonblank, noncomment lines before test modules). Egress's commit gate stays unchanged.
  - Full suite passed twice on clean restored code: `env -u RUST_BACKTRACE cargo test --locked -p nanovm`, 93 unit and 64 integration passed, one ignored. Earlier load-sensitive failures passed unchanged reruns (N-32).
  - Seven mutations failed their tests: pinning the old digest, deleting the identity guard, disabling the global cap, extending the publish deadline, cross-object fan-out, dropping record-owner identity, and permitting forged client events. Each restored with `git checkout -- <file>` and an empty diff confirmed before clean-suite validation.
  - Adversarial subagent review found no confirmed runtime blocker. Owner/repository/alias test gaps were fixed. N-33 (initial DNS lookup deadline) and N-34 (malicious-worker header test) remain recorded follow-ups. The tool could not attest to Opus routing.
  - Demo: 5,000 cold + 50,000 warm + 10,000 writes + 5,000 serial requests all succeeded; 100 attacks contained; canary 543 requests, zero failures. Started/stopped via `amp orb service`; no services left running.
  - Quick release benchmark, baseline → cleanup requests/s: cold 3318→3353; warm concurrent 69196→67023; serial GET 9747→9924; concurrent writes 574→580; serial writes 287→304; trap recovery 1368→1309; churn 5313→5536; neighbors under attack 54900→61185. Density 96 KB/object both. Mixed variation on one before/after run, not proof of no performance regression; this measures the in-process node, not proxy throughput.
  - **Implementation handoff:** Read Q53–Q55 and `e62e9ac`; preserve current-digest event routing, app identity, and committed publish forwarding when simplifying.
  - **Cleanup/successor handoff:** Start on main; read HANDOFF, ISSUES, README and PLAN. W-3 and N-14/N-20 are complete. Next roadmap item is W-2 (OpenTelemetry); N-33/N-34 are recorded follow-ups. Claim a branch and update docs at milestones; leave guests unchanged without owner approval.
- **Next:** PLAN.md's Roadmap: OpenTelemetry by default (W-2), the per-object SQLite demo, alarms outside the node, data lifecycle planning, then the groundwork for many nodes.
- Branches merged into `main` by fast-forward are not always pushed on their own. The remote `config` branch is stale: it was merged by a fast-forward of a rebased copy.
- **Tests:** 64 integration tests and 93 unit tests pass. One is ignored: `egress_listed_host_allowed` needs external HTTPS; it succeeded in the demo. The registry tests run against the real CI-signed guests on ghcr.io.
  - Tests that do heavy file work under `hostile`'s 100 ms CPU budget (wall time inside guest polls) can be killed when the whole suite loads the machine; such tests use `filler()` (1 s). See N-32.
  - `cpu_hogs_leave_cores_for_short_requests` is load-sensitive and fails occasionally when the whole suite runs at once on 4 cores; it passes alone.
  - When guests change, the first run can fail one supervised test with `Permission denied`. The test nodes share one `.cwasm` cache between root in-process nodes and jailed workers. A rerun passes.
- **Runtime size:** about 5,250 lines before phase 7, which added the proxy and a registry client of our own (3,704 after phase 3; kv and config added about 300, WebSockets about 640, commits about 500 non-blank, non-comment lines, against an estimate of 250–300, which the owner accepted after a reduction pass).
- **The repo `carsonfarmer/nanovm` is now public**, so GitHub artifact attestations work. The packages `ghcr.io/carsonfarmer/nanovm-{counter,hostile}` are public. `publish-guests.yml` runs on pushes to `guests/**` and on `gh workflow run publish-guests.yml` (dispatch works through the proxy, and `gh run watch <id>` shows progress). Each run publishes version `1.0.<run number>`.
- **Old branches** (`jail`, `simplify`, `oci`, `loc-reduction`, `linux-baseline`, `quota-rename-self-repro`) are merged or historical.

## Next up: W-2, OpenTelemetry

The owner requested W-2 after enabling CI. The first code milestone decouples worker readiness from `/_nanovm/stats`: `Supervisor::start` now requests the private `/_nanovm/ready` response (204, no snapshot, no guest dispatch). The extended `http_front_end` regression failed with 404 before implementation. Keep the deprecated APIs until their OTel replacements and demo/test consumers work, then delete them (no compatibility layer).

Readiness validation: full locked suite with `RUST_TEST_THREADS=1` passed (93 unit, 64 integration, one ignored). Deleting the readiness handler made `http_front_end` fail with 404; restored with `git checkout -- crates/nanovm/src/server.rs`, confirmed an empty diff, and the same test passed. No guest changes or new telemetry dependencies yet.

The readiness milestone also passed GitHub CI run 41228340612 at `b95d144`. The demo passed all 70,000 normal requests and contained 100 attacks; its canary had 533 requests and zero failures. The demo service was stopped afterward. This milestone is pushed on `w2-observability`, not merged into main; CI itself is on main at `617668e`.

Implementation plan from source inspection:
- Use `opentelemetry`, `opentelemetry_sdk` and `opentelemetry-otlp`, rather than invent a telemetry encoding. The OTLP HTTP exporter accepts a custom `opentelemetry_http::HttpClient`; a worker client can send through its existing egress link. Only the supervisor reaches the collector. `tracing-opentelemetry` bridges spans, but does not export logs: use the logs SDK for guest stdout/stderr. Docs inspected: https://docs.rs/opentelemetry-otlp/latest/opentelemetry_otlp/ and https://docs.rs/tracing-opentelemetry/latest/tracing_opentelemetry/ . Resolve compatible crate versions at implementation time.
- The supervisor owns collector configuration and credentials. A worker must not choose an arbitrary export URL or claim another app/version. Decode bounded OTLP batches, replace tenant resource identity from the worker's trusted supervisor record, then export. Guest HTTP must not access this internal telemetry route. Test these boundaries with crafted requests, including duplicate/spoofed attributes.
- Bound SDK queues, batch bytes, forwarding concurrency, export deadlines and metric cardinality. Collector failure must drop/report telemetry, not block requests or retain unbounded data. Do not weaken the jail for SDK network access. Verify SDK background-thread/custom-client runtime behavior before integrating.
- Instrument dispatch/queue wait, instantiation, guest handling, commit and object calls. Pass trace context explicitly across actor queues and links; do not hold thread-local span guards across awaits. Tag app/object/digest; distinguish stdout and stderr while keeping bounded panic-tail diagnostics. Counters needed for live-store admission remain runtime state, not an OTel round trip.
- Replace stats/events consumers in the demo and integration tests with a local OTLP collector fixture; preserve worker death, version-switch and isolation assertions. Worker readiness becomes a small private response, not an exported metric. Delete HTTP stats/events and the event ring only once equivalent checks exist.
- Validate real jailed-worker exports for all three signals, tenant/version isolation, collector outage, telemetry flood, trace parentage and guest logs. Mutation-check those contracts, run the full suite/demo/benchmark, and review the telemetry trust boundary before landing.

**First-piece handoff:** Continue `w2-observability` with bounded OTLP transport; the readiness prerequisite is validated. The plan above is not an assertion that telemetry already exists. Do not change guests without approval.

## Completed W-3 brief (historical starting point)

The following records the pre-W-3 implementation and plan. W-3 is complete; current behavior and validation are above, with decisions Q53–Q56 below.

**Goal.** The node only ever answers requests. The front proxy (`nanovm proxy`) holds client WebSockets, as Pushpin does, and turns each socket event into the plain `POST` of `application/websocket-events` that objects already get. Objects and guests don't change: they keep answering events in Pushpin's WebSocket-over-HTTP format and publishing GRIP messages to `http://nanovm/publish/`. Most of `ws.rs` (about 1,140 lines) leaves the node, and N-14 and N-20 go with it.

**How it works today** (read these first):
- `ws.rs`: `Sockets` (per worker: open sockets, channels, caps), `Sockets::open` (upgrade, `OPEN` round trip, then `run`, the per-socket loop: one event in flight, a write backlog, ping, stall timeout), `Sockets::publish` (GRIP publish, fan-out to the object's own subscribed sockets), `answer` and `parse` (the event wire format), `Caps`.
- `supervisor.rs`: `handle` refuses client requests of type `application/websocket-events` (`ws::is_event`) and sends upgrades (`ws::wants`) to `socket()`, which opens the socket on the app's current `Live` and delivers its events over the worker link (`Deliver`). Each `Live` owns a `Sockets`; they close (1012) when the worker exits or the app switches versions. The egress link serves `http://nanovm/publish` from the worker by calling `Sockets::publish`. `slot()` (N-22) treats an app with open sockets as busy.
- Worker side: a guest's publish is intercepted in `egress.rs` (`send`, then `hold`: size bound, `well_formed`, stamped with `nanovm-object`), held until the request commits, and sent to the supervisor by `object::commit`. `table.rs` `enqueue` lets a socket event wait for room in a full queue instead of being refused.
- The proxy (`proxy.rs`) today passes upgrades through to the node with `copy_bidirectional`.
- Tests: `websockets_reach_their_object_and_fan_out_within_it`, `websocket_limits_hold`, the WebSocket part of `the_proxy_routes_hosts_by_dns`, and the unit tests in `ws.rs`. README's "WebSockets" lists every rule and limit the tests pin.

**The shape to aim for** (decide the details, and record them as Q-numbered decisions in the table below):
- **The proxy holds the sockets.** On an upgrade it sends `OPEN` to the node as a `POST` with the app headers it already sets, keeps the socket if the object answers `OPEN`, and posts each later event the same way. Keep the current rules: one event in flight per socket, the write backlog and stall limits, pings, the size bounds, and an object's 4xx/5xx refusal reaching the client.
- **Publishes go to the proxy.** The proxy needs a publish endpoint that only the node can reach (a second, private listener, like Pushpin's control port), and the node needs to know where it is (a node config key). The supervisor still makes the request, since it owns the network; the worker side (`hold`, held until commit) stays. A publish must name its app and object, and the proxy must scope channels to `(app, object)`, so one app can never reach another's sockets. The app identity the proxy uses must be the same one the node uses (the record owner's name, `Route.owner`, since W-15).
- **Only the proxy sends events.** The check that refuses a client request of type `application/websocket-events` moves to the proxy (strip or refuse it before forwarding); the node then trusts events as it trusts the other `nanovm-*` headers, because only the proxy can reach it.
- **What leaves the node:** `Sockets`, `socket()`, `Live.sockets`, `ws::Caps`, the node's `max_sockets` settings (they become proxy settings), the publish handler on the egress link (it becomes a forward to the proxy), and the open-sockets check in `slot()`. What stays in `ws.rs`, if anything, is the event wire format, which the proxy and the worker share; move it wherever is simplest.
- **Lifetime changes, so decide it with the owner in mind:** sockets would now outlive workers, so a version switch no longer has to close them. Either keep closing them on a switch (the proxy sees the digest change) or let them carry over to the new version; write down which, and why.
- **Pushpin itself** was named as an alternative (run Pushpin in front instead of growing `proxy.rs`). If you consider it, compare the two briefly and say why you chose; the owner prefers the fewest moving parts.

**Concrete plan (worked out from the code, 2026-10-31; follow it unless you find a reason not to):**
- **Q53, decided: sockets carry over a version switch.** They live in the proxy keyed by app name, and each event is sent with the app's *current* digest (re-resolve the route and digest for every event, from the proxy's caches). Pinning the digest at `OPEN` would make the node switch the app back to the old version on every event. So the worker-gone signal goes away: drop the `gone` parameter of `Sockets::open` and `run`, `Reply::Gone`, and the unit tests that use them (about lines 594–598, 956 and 1064 of `ws.rs`). If the node can't be reached, the event gets `Reply::Bad(502)` and the socket closes with 1011.
- **`ws.rs` stays a shared module, now used by `proxy.rs`.** `Sockets`, `Conn`, `run`, `publish` and the wire format move over unchanged apart from Q53, because `Deliver` already hides where events go.
- **Proxy:**
  - Add `sockets: Mutex<HashMap<String, Arc<Sockets>>>`, keyed by `registry::app_name(&route.owner, &route.app)`; this is the name the node uses. When inserting, drop entries whose `is_empty()` is true, so the map stays bounded by open sockets.
  - Factor the header-setting part of `forward` into a `resolve(host)` that returns the app headers.
  - In `forward`: refuse a client request with `ws::is_event` (400; the check moves here from `supervisor::handle`). For `ws::wants`, take the object from `table::route`, then call `Sockets::open` with a `Deliver` that runs `resolve(host)` again, sends the event `POST` to the node with the app headers, and returns `ws::answer(res, caps.response_max)`. `forward` needs `self: &Arc<Self>` for that.
  - Make the socket caps proxy settings: total, per app, per object, message size (the old `max_request_body_kb`), answer and publish size (the old `max_response_body_kb`), and `socket_backlog_kb`. Keep the current defaults.
- **Publish path:**
  - Add a second, private listener, `publish_listen` (default `127.0.0.1:8444`), served by a `serve_publish`. A `POST /publish` naming the app (a header) and carrying `nanovm-object` goes to that app's `Sockets::publish`.
  - Add a node config key, `publish_to` (the proxy's publish URL). The supervisor's egress link no longer calls `Sockets::publish`; it forwards the held publish there (over reqwest with `.no_proxy()`), adding the app's name.
  - `main.rs` binds both proxy listeners.
- **Removed from the node:** `supervisor::socket`, `Live.sockets`, `ws::Caps` construction, `Supervisor.sockets`, the `is_event`/`wants` branches in `handle`, the open-sockets check in `slot()`, and `sockets` in stats. Also the socket settings: `NodeConfig.max_sockets`, `WorkerLimits.max_sockets`, and `Limits.max_sockets` and `Limits.socket_backlog_kb`. Check `guests/*/nanovm.json` before removing `Limits` fields: `deny_unknown_fields` would reject a guest config that still sets them, and changing guests needs the owner's OK.
- **Tests:** `websockets_reach_their_object_and_fan_out_within_it` and `websocket_limits_hold` (in `tests/integration.rs`) connect to the node directly today. Route them through the proxy with a helper that binds the proxy's publish listener first, starts a supervised node with `publish_to` set, then starts the proxy pointing at it, and gives each app a DoH record. Drop the "a worker's sockets close when it goes" step (Q53), and keep "a forged client event is refused" (now by the proxy). Add a test that one app's publish never reaches another app's sockets.
- **Then a simplification and cleanup pass** (the owner asked for one): re-read `ws.rs`, `supervisor.rs`, `egress.rs` and `proxy.rs` for code left over from the move. Report the runtime line count before and after.

**Constraints:**
- Guests stay as they are. Changing `guests/**` and pushing publishes new guest versions through CI, so ask the owner before doing that.
- The node keeps its existing guarantees: per-object ordering of events with other requests, commits before publishes, and the caps that keep the node's memory bounded.
- The proxy faces the public: every per-client resource it holds (sockets, backlogs, channels) needs a bound, as N-22 to N-30 did for the node. Expect the reviewer to try exactly that.
- The runtime should get smaller overall; report the line count before and after (non-blank, non-comment lines of `crates/nanovm/src`, without `#[cfg(test)]` modules).

**Done means:** the WebSocket tests pass through the proxy (moved from the supervisor), plus new ones for the proxy's bounds and for cross-app isolation of channels; the full suite and the demo pass; benchmarks don't regress (`cargo run --release --example bench -- --quick`); an adversarial review by a subagent, with its findings fixed or recorded in ISSUES.md; README ("The proxy", "WebSockets", "Tests"), PLAN, HANDOFF and ISSUES (W-3, N-14, N-20) updated in the same push.

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
| Q51 | Clean up the review's removable-code list (W-13, done). |
| Q52 | Phase 7 as built: a host's app comes from its `_nanovm.<host>` TXT record over DNS-over-HTTPS, as a tag the proxy resolves to a digest; apps only from registries, one per worker; apps keyed by the name that owns their `_nanovm` record, after CNAMEs, and `registry/repository` (by repository alone at first, until the review showed any domain could then pick another's version; then by request host, until the owner chose record owners so aliases share one app, W-15); a loopback-only dev mode without signature checks for tests and the demo; our own size-capped registry client instead of oci-client's downloads. |
| Q53 | Sockets live in the proxy keyed by app and carry over worker exits and version switches. Every event resolves the current digest through the proxy caches; never pin the OPEN digest and switch the app back. |
| Q54 | A socket closes with 1011 if its host changes app identity (record owner or repository); events never cross apps. Event delivery has a 240 s overall deadline, allowing cold deploys but bounding stuck nodes/resolvers. Publishes have a 30 s body/forwarding deadline. |
| Q55 | Keep tungstenite and the existing GRIP implementation rather than add a Pushpin process. Proxy socket caps are operator settings, not guest manifests. The private publish listener trusts network isolation, like the node; only the supervisor stamps the app identity. |
| Q56 | Socket managers share the proxy config directly, with no copied `Caps` model. A BTreeMap keyed by `(object, connection-id)` makes fan-out visit only the publishing object's sockets, without a second index or per-object counters (N-20). Pending opens and connections hold the app manager alive; its Arc count also identifies removable map entries. |
| Q57 | Worker readiness is a private 204 response on the request link, independent of stats and telemetry. Removing the deprecated stats/events APIs must not make startup depend on a collector. |

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
- **The soft fd limit is 1,024** (hard limit 1,048,576): run `ulimit -n 100000` before the demo or the benchmark. The supervisor and workers raise their own soft limit to the hard one at start, but the shell's limit still bounds the load generators.
- **The demo works in orbs** since N-21. Since phase 7 it publishes the guests to a loopback registry (`examples/dev-registry.rs` on 127.0.0.1:5000) and runs a dev-mode node; `pkill -x dev-registry` and `pkill -x nanovm` clean up after it. Outbound HTTPS works directly (the egress test to example.com passes in the demo).
- **`strace` installs with `sudo apt-get install -y strace`**; it is how the seccomp allowlist was traced (`strace -f -e trace=<syscall>` over the integration tests).
- Everything below about the earlier cloud sandbox still holds unless an orb note above says otherwise; `RUST_BACKTRACE` isn't set in orbs, but keep using `env -u RUST_BACKTRACE`.

## Environment gotchas (the earlier cloud sandbox)

- **`vm.max_map_count`** resets when the container restarts. Run `sysctl -qw vm.max_map_count=1048576` before tests or the demo; the worker's startup check names the fix if you forget.
- **The fd limit is 20,000** and can't be raised (no `CAP_SYS_RESOURCE`). The demo uses `max_live_objects = 8000` per app, and the full benchmark needs `--quick`.
- **Never run with `RUST_BACKTRACE` set** (`env -u RUST_BACKTRACE`). wasmtime captures a backtrace for every guest error, which costs about 3×.
- **cgroup v2 here has only hugetlb**; memory, cpu and pids belong to cgroup v1. Use `scripts/vm-test.sh` (QEMU with software emulation, an Ubuntu kernel) to test cgroups. CPU-budget and latency tests fail under emulation, so the default set avoids them.
- **No DNS and no direct internet**; outbound HTTPS goes through a proxy the egress code doesn't use. `ghcr.io`, github.com and `pkg-containers.githubusercontent.com` (where ghcr.io redirects blob downloads; the owner allowed it) work through it, but sigstore.dev doesn't. The registry client then honoured the proxy; since phase 7 it is our own (over reqwest) and ignores any system proxy (`.no_proxy()`, so its public-address check holds), so there it would need direct access. In orbs ghcr.io is reached directly.
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
  - `proxy.rs` (`nanovm proxy`), `supervisor.rs` (supervisor and worker entry), `registry.rs` (verification and the size-capped registry client), `link.rs`, `jail.rs`, `ws.rs` (WebSockets), `kv.rs` (`wasi:keyvalue`), `journal.rs` (per-request commits)
  - `egress.rs` (`Route::Direct` in-process, `Route::Supervisor` jailed; also calls between objects and held publishes)
  - `object.rs`, `table.rs` (`Node`), `quota.rs`, `server.rs`, `config.rs`, `engine.rs`, `app.rs`, `metrics.rs`, `main.rs`
- `crates/nanovm/tests/integration.rs`: the integration tests. Helpers: `node_cfg`, `supervised_with`, `http_get`, `test_cgroup`, `stalling_upstream`. `jail_probe` runs in a child process.
- `crates/nanovm/support/load.rs`: the in-process load generator, used by the tests and the benchmark.
- `crates/nanovm/examples/bench.rs`: the in-process benchmark. `examples/dev-registry.rs`: a loopback registry with the guests published, for the demo and local development.
- `guests/`: `counter`, `hostile` and `util`; build them with `scripts/build-guests.sh`.
  - Hostile endpoints include `/hold` (keeps response bodies open), `/big` (a large body) and `/commit` (writes, then ends as `then=` says).
  - The counter guest has a chat room on `/ws` and `POST /publish`.
- `demo/demo.sh` and `demo/nanovm.toml`.
- `scripts/vm-test.sh`.
