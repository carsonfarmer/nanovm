# Nice-to-have backlog for an independent agent

Written 2026-11-08 against `main` at `1e5f18a`. These are things nanovm wants but nobody on the core
path has time for. None of them should block or reshape the core work below; if an idea starts to,
stop and write it up instead.

## What nanovm is (derived from the code)

A self-hostable, multi-tenant runtime for untrusted web backends written as WASIp3 HTTP components.
Every `/{object}/…` URL names a **durable object**: a single-threaded Wasm actor, started on demand,
evicted when idle, with its own memory, CPU, deadline, disk and egress limits and a private
filesystem volume whose changes **commit atomically and durably per request** (or roll back). The
host's trusted surface is small — a quota'd, journaled filesystem, outbound HTTP through an
allowlist, and config; key-value and SQLite are Wasm components composed over that filesystem. Apps
are signed OCI artifacts named by DNS, run in per-app jailed worker processes behind a supervisor
that alone touches the network. Today it is one node behind one proxy.

## Core work owned elsewhere — do not touch

1. Landing the security fixes (N-70..N-75) and wasmtime 50.0.1 on `main`.
2. A commit stream off the node (redo records → object storage, snapshots, restore): `journal.rs`,
   `quota.rs`, the commit path in `object.rs`, `scripts/backup.py`.
3. Multi-node placement with fenced leases: `proxy.rs` routing, `supervisor.rs`, `table.rs`,
   `registry.rs`.
4. Cheaper durable writes (fewer fsyncs, group commit): the same storage files.
5. The developer on-ramp (`nanovm run`, Rust SDK, JavaScript, `nanovm publish`) from branch
   `cf/optimistic-feynman-62f1a0`.

## Ground rules

- One branch per idea, `ideas/<slug>`, from the latest `main`. Small, reviewable changes. Never push
  to `main`.
- Prefer new files and directories: `examples/`, `components/`, `tools/`, `experiments/`, `docs/`.
  Touch `crates/nanovm/src` only where an idea says so, in small isolated hunks; runtime lines
  (CI's count) must not grow without a measured reason.
- Don't edit `guests/**` (pushing it to `main` republishes the test guests). Put new apps elsewhere.
- No repo-wide reformatting, renames or moves; they conflict with everyone's branches.
- Ask the owner before downloading or running third-party binaries or publishing anything.
- Every change carries evidence: a test that fails without it, or an A/B measurement (alternate old
  and new builds, report medians and ranges). Write results in plain language.
- Record findings in `ISSUES.md` using only IDs N-500+ / W-500+.

Ideas are tagged by size: (S) under a day, (M) a few days, (L) a week or more.

---

## A. Example apps

Each lives under `examples/<name>/`, has a README with one run command, and an end-to-end test that
proves it works, including persistence across eviction and a restart.

1. **Multiplayer game lobby** (M): tic-tac-toe or chess rooms as objects, matchmaking through calls
   between objects, live moves over WebSockets, move validation in the room.
2. **Collaborative editor** (L): one document per object using a CRDT (`yrs` or `automerge` built for
   wasm32-wasip2), WebSocket sync, update log plus periodic snapshot compaction.
3. **URL shortener with analytics** (S): links sharded across objects, click counts rolled up per day.
4. **Rate limiter service** (S): token bucket per key object; shows why serial objects make this trivial.
5. **Leaderboard** (S): SQLite-backed top-N with pagination.
6. **Job queue with retries** (M): queue object, workers pull through calls, idempotency keys,
   dead-letter queue, visibility timeouts done by client polling.
7. **Webhook receiver and relay** (M): verify GitHub/Stripe HMAC signatures, store, fan out over
   egress with retries; tests use a local upstream.
8. **Feature-flag service** (S): flags per tenant object, `wasi:config` defaults, live updates over
   WebSockets.
9. **Pastebin with server-side highlighting** (S): `syntect` in Wasm under the CPU budget.
10. **Wiki with history** (M): markdown pages (`pulldown-cmark`), revisions in SQLite, diffs.
11. **Image thumbnailer** (M): the `image` crate in Wasm, cached results in `/data`; shows the memory
    and CPU limits doing their job on large images.
12. **Presence and typing indicators** (S): per-room presence over WebSockets.
13. **Polls and voting** (S): per-user dedupe, live tallies.
14. **OAuth login and sessions** (M): GitHub OAuth over egress, secrets in `wasi:config`, session
    cookies, a session object per user.
15. **Analytics collector** (M): ingest events, per-day rollups, CSV export.
16. **Lock and lease service** (S): one object per lock; demonstrates single-writer semantics.
17. **LLM gateway** (M): per-user quotas and streamed responses from an upstream API (mocked in tests).
18. **Feed aggregator** (M): fetch RSS/Atom over egress, dedupe, per-user feeds.
19. **Notes with full-text search** (M): a small inverted index (or `tantivy` if it builds for WASI).
20. **Cart and inventory** (S): transactional SQLite; a failed checkout rolls everything back.
21. **Counter in every language** (S): the same API in Rust, JavaScript, C and anything else that
    builds, for an apples-to-apples overhead table (see E-65).
22. **Static site plus API in one component** (S): embedded assets with ETag and Cache-Control.
23. **Offline-first todo sync** (M): one object per user, client-side queue, conflict handling.
24. **Server-Sent Events** (S): a streaming response example and a test of how post-return work and
    deadlines behave with long-lived streams.
25. **Uploads and downloads** (S): streaming bodies, quotas, HTTP range requests.
26. **Reminders without alarms** (S): the best pattern available until alarms exist (client polling
    or an external cron hitting an object), documented honestly.

## B. Languages and toolchains

27. **Python guests** (L): check componentize-py's WASIp3 status; if it isn't ready, prototype an
    embedded interpreter runtime like `js/` and measure it.
28. **Go guests** (M): TinyGo/Go WASI status for components and wasi:http@0.3; a hello-world if possible.
29. **C and C++ guests** (S): counter with wasi-sdk 34 and `wit-bindgen` C bindings.
30. **Zig guest** (S): the same counter.
31. **Wasm-GC languages** (M): a Kotlin/Wasm or Dart spike, since the GC proposal is enabled.
32. **JavaScript extras** (M, after the on-ramp lands): streaming bodies, `performance.now`, TypeScript
    types for `env`, an esbuild bundling script so npm packages work.
33. **Workers/Durable Objects API shim** (L): `state.storage.get/put/list/transaction` over the KV
    component, so existing Workers code ports with small changes.
34. **Spin app compatibility** (M): map `spin.toml` to `nanovm.json` and run unmodified Spin examples
    that use `spin:key-value` / `spin:sqlite`; document what differs.
35. **Project templates** (S): `cargo generate` templates and a `tools/new.sh` for Rust and JS apps.
36. **Rust SDK extras** (M, after the SDK lands): a router, typed JSON handlers, middleware, and a test
    helper that runs a component against an in-process node in `cargo test`.

## C. Developer tooling (outside the runtime crate)

37. **`tools/inspect`** (S): print a component's imports and exports, flag imports nanovm refuses,
    show its composition and declared limits.
38. **Volume export and import** (S): tar one object's `/data` consistently (holding the app lock) for
    debugging and migration; import into a fresh root.
39. **Request recorder and replayer** (M): capture requests to an object and replay them against a
    new version to diff responses.
40. **One-command local stack** (M): proxy, node, dev registry and a fake TXT resolver together, for
    testing routing and WebSockets locally.
41. **Load scenarios** (S): oha/k6 scripts per example app with expected numbers.
42. **Debugging guide** (S): guest debug info, wasmtime's debugging options, reading traps and the
    `x-nanovm-error` header; editor configs.
43. **GitHub Actions template for apps** (M): build, compose, push, sign with an artifact attestation,
    print the TXT record (complements W-7 without touching the runtime).
44. **`nanovm doctor` script** (S): check `vm.max_map_count`, fd limits, Landlock, cgroup v2
    controllers and filesystem features, and say how to fix each.

## D. Testing and verification

45. **Fuzzing** (M): `cargo-fuzz` targets for the WebSocket event parser, GRIP publish parser, TXT
    record parser, OCI reference parsing, journal `decode`, telemetry stamping and config parsing;
    a nightly CI job.
46. **Model-based quota test** (M): random sequences of write/truncate/rename/unlink/mkdir/rmdir with
    commits and rollbacks against a simple model; usage must always equal the model's.
47. **Power-loss simulation** (L): a fault-injecting filesystem (LazyFS, dm-flakey or a FUSE shim)
    that drops unsynced writes, to check journal recovery beyond process kills. Report only; the
    storage owners decide what to change.
48. **Concurrency model checks** (M): `loom` or `shuttle` tests for actor retire/enqueue handshakes and
    any custom wakers that land.
49. **Miri and sanitizers** (S): Miri on pure modules, ASan/TSan nightly builds.
50. **Seccomp drift check** (S): run the suite under `strace -f`, diff syscalls against the allowlist,
    warn in CI when a dependency upgrade needs a new syscall.
51. **Soak test** (M): 24 hours of churn and mixed traffic; RSS, fds and mappings must stay flat
    (PLAN lists leak tests as not written).
52. **Deploy-time validation test** (S): a component importing `wasi:sockets` is refused, by name (also
    listed as not written).
53. **Faster, steadier test suite** (M): profile the slowest integration tests, try `cargo-nextest`,
    and fix or quarantine load-sensitive timing tests with their causes recorded.
54. **Flake tracking** (S): a weekly CI job that runs the suite N times and records per-test failure
    rates.
55. **Coverage report** (S): `cargo-llvm-cov` as a CI artifact; list untested branches in the
    journal, quota and proxy code.
56. **Mutation testing** (M): `cargo-mutants` on `ws.rs`, `registry.rs`, `egress.rs` and `jail.rs`;
    write tests for surviving mutants.
57. **cgroup tests in CI** (M): run `scripts/vm-test.sh` in a CI job with nested virtualization, or
    document a self-hosted runner.
58. **Hostile guest, second edition** (M): a new test-only component (not `guests/**`) with hash
    flooding, runaway recursion, pathological regexes, huge header counts, slow uploads, outbound
    connection storms, rename storms, maximum-length and unusual-Unicode paths.

## E. Benchmarks and evidence

59. **Benchmark history** (M): run `bench --quick` per `main` commit on a quiet runner, store JSON,
    chart trends, flag regressions over 5%.
60. **Comparisons against Spin, `wasmtime serve` and workerd** (M, W-8): needs the owner's approval to
    run their binaries; equivalent apps, pinned cores, published method.
61. **Density at scale** (M): 100k+ live objects on a large VM; memory, fds and mappings per object;
    find the next limit.
62. **Compile-time study** (M): Cranelift opt levels against Winch and Pulley for a never-seen version
    (compile time against run speed); informs scale-to-zero and multi-node first requests.
63. **Filesystem matrix** (S): ext4, XFS, btrfs and tmpfs, with and without `discard`, for write-heavy
    requests; measurement only, for operator docs.
64. **Tail latency under attack** (S): mixed hostile and benign HTTP load; publish latency histograms.
65. **Per-language overhead table** (S): from A-21.
66. **A public benchmark page** (S): method, raw data and charts, regenerated by script.

## F. Observability

67. **Dashboards and alerts** (S): Grafana dashboard JSON and Prometheus rules for the derived span
    metrics, span drops and Collector saturation.
68. **Collector recipes** (S): configs for Honeycomb, Grafana Cloud, Datadog and a local Jaeger.
69. **Guest instrumentation prototype** (M, W-17): a WIT interface implemented by a composed component
    that emits structured lines the host already correlates, so no host change is needed.
70. **Per-object usage rollups** (M): CPU time, disk bytes and requests per object from existing
    telemetry, aggregated in the Collector into billing-ready records.
71. **"Why did my object die?" guide** (S): every `x-nanovm-error` reason with causes and fixes.

## G. Security extras (defense in depth)

72. **Supply-chain checks** (S): `cargo-deny`, `cargo-audit` and license checks in CI.
73. **SBOM and reproducible release builds** (M).
74. **Landlock scoping** (S): signal and abstract-socket scoping (ABI 6+) as an extra layer; measure
    and test.
75. **Proxy abuse limits design** (M): per-client connection and request limits (README notes they're
    missing); design and benchmark only, since proxy routing is core work.
76. **Threat model** (S): boundaries, assets and attackers, from `notes/sec.md` on the branch above.
77. **Signed nanovm releases** (M): sign release binaries with Sigstore.
78. **App secrets design** (M): encrypted `wasi:config` values and rotation, as a design and a
    prototype outside the runtime.

## H. Simplification and housekeeping (coordinate before starting)

79. **Docs diet** (M): HANDOFF is 154 KB and README 77 KB. Propose a one-page current state, a short
    README, and move history to `docs/history/`. Docs-only branch, merged quickly, owner approves.
80. **macOS decision** (M): nanovm no longer compiles on macOS (Linux-only flags in `link.rs`). Make it
    portable and verify with a macOS CI runner, or propose removing the dead macOS branches.
81. **Clippy cleanup** (S): existing diagnostics, only in files no one is changing.
82. **CI speed** (S): split unit, integration and SQLite jobs; better caching; nextest.
83. **Retire experiments** (S): fold `experiments/*` conclusions into short READMEs and delete dead code.
84. **Line-count honesty** (S): move test-only helpers below CI's count cut so it measures runtime.

## I. Protocol and edge features (isolated)

85. **WebSocket compression** (M): permessage-deflate in the proxy, behind a setting; measure CPU and
    bandwidth.
86. **Trace context to guests** (S): pass `traceparent` and a request id to guests if they aren't
    already visible.
87. **SDK middleware** (S): CORS, security headers, cookies, range responses.
88. **Caching proxy design** (M): Cache-Control-aware caching of GETs at the proxy; design and
    prototype only.

## J. Wasm components over the filesystem (zero runtime lines)

89. **JSON document store** (M): collections and secondary indexes.
90. **Queue / stream** (M): append-only log with consumer offsets.
91. **Full-text search** (M).
92. **Vector index** (M): HNSW, with a semantic-notes example.
93. **Time series and rollups** (M).
94. **CRDT documents** (M): with a WebSocket sync helper (pairs with A-2).
95. **Rate limiting** (S).
96. **HTML templating** (S): `minijinja`.
97. **Auth toolkit** (M): JWT/JWKS verification, argon2 password hashing within CPU limits, sessions.
98. **Image processing** (M).
99. **Email and SMS clients** (S): provider APIs over egress.

## K. Research spikes (report, don't adopt)

100. **Pre-initialization for Rust guests and SQLite** (S): measure Wizer on them.
101. **`pagemap_scan` slot reuse** (S, W-26).
102. **Memory protection keys** (M): pooling with large reservations on hosts with `pku`.
103. **Upstream the byte-list copy patch** (M): turn `experiments/wasmtime-byte-list-copy.patch` into a
     wasmtime PR (owner approves before anything is filed).
104. **Next wasmtime / WASI 0.3.x** (S): track releases; prepare an upgrade branch with test results.
105. **io_uring for storage I/O** (M): measurement only; hand results to the storage owners.
106. **Component-model async overhead** (M): profile guest↔host calls; file findings upstream.
107. **eBPF accounting** (M): per-app CPU, I/O and network accounting without touching the runtime.
108. **gVisor and Firecracker comparison** (M): isolation cost against the jail.
109. **Instance snapshots** (L): feasibility of serializing a warm instance (memory, globals, tables)
     to resume elsewhere.

## L. Operations and packaging

110. **Container image** (S): minimal image for `nanovm serve` and `proxy`, with required sysctls
     and limits documented.
111. **systemd units** (S): hardened units, sysctl drop-ins, `LimitNOFILE`.
112. **Nix flake** (S).
113. **Single-node reference deployment** (M): Terraform or Ansible for node, proxy, Caddy TLS and a
     Collector on one VM.
114. **Capacity calculator** (S): objects → fds, mappings, memory reservation, disk.
115. **Runbooks** (S): upgrade, rollback, backup/restore with `scripts/backup.py`, disk full, worker
     crash loops.
116. **Release process** (M): versions, a changelog from commits, GitHub releases with binaries.

## M. Documentation and learning

117. **Tutorials** (M): build a chat app, add SQLite, deploy with DNS, debug a killed object.
118. **Architecture explainer** (M): diagrams of the request path, the jail and commits.
119. **Comparison page** (S, after E-60): nanovm against Workers/Durable Objects, Spin, Fly and Deno
     Deploy, with evidence for every claim.
120. **FAQ and glossary** (S).
121. **"How commits work" interactive page** (M): an animation of the undo journal, commit and rollback.

## How to pick

Start where results show quickly and nothing depends on core work: D-45, D-52, D-50, G-72, A-3, A-4,
A-21, E-59, F-67, L-110. Leave anything marked "after the on-ramp lands" until it has.
