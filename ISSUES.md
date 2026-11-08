# Issues

One list of known problems (`N-n`) and planned work (`W-n`), so nothing is done twice or missed. Before starting one, set its status to `fixing` (or `in progress`), with who has it, and push; when it is on `main`, set `fixed in <commit>` (or `done in <commit>`) in the same push. New problems get the next `N-` number, new work the next `W-` number.

- **Reproduced**: shown against the real code or the OS. **From source**: traced through the code, not run.
- Status: `open`, `fixing`, `fixed in <commit>`, or `deferred: <why>`.

**Current delivery (2026-11-08):** backup/restore and N-43 are delivered through `1e5f18ad`, with passing CI. W-9's bounded committed-version proof is `310c992`, validated by CI 42438468359 and included in main integration; no production runtime files are owned or changed. Runtime remains 5,085; 5k is a simplicity goal, not a hard limit. Both slower actor rewrites remain rejected. Live capture, distributed fencing and automatic failover remain unimplemented. W-5 records remaining lifecycle scope, not the next priority.

## Problems

Found by the review at `c607ec9`, unless an entry says otherwise; line numbers are as of that commit.

### N-1: a registry app's egress list can name private and loopback addresses
- **Where:** `egress.rs:26-28`, `egress.rs:313-314`, `config.rs:128-136`, `supervisor.rs:322,333`
- **Scenario:** An IP-literal egress rule skips the public-address filter, because the operator is assumed to have written it. For a registry app the tenant writes it: `Limits::clamped` keeps the app's own `egress`. A signed `nanovm.json` naming `127.0.0.1:<port>` or `169.254.169.254` reaches the host's private network, including the node's front end, where the app can send the proxy-only headers and switch another app name to a version it signed.
- **Reproduced:** no (from source)
- **Fix:** refuse non-public IP literals in tenant-supplied egress lists. Once local apps are gone, that is every list, and the exemption in `egress.rs` can go (the egress integration tests that allow `127.0.0.1` need another upstream).
- **Status:** fixed in bbeb543

### N-2: the worker's syscall filter allows every `fcntl` command
- **Where:** `jail.rs:131`
- **Scenario:** `F_SETOWN` with `O_ASYNC` (and `F_SETSIG`) on a descriptor a worker holds makes the kernel signal a process of the worker's choosing when I/O arrives; the kernel checks only that the uids match. When the supervisor isn't root, workers share its uid, so code that got out of Wasm can stop the supervisor and other apps' workers.
- **Reproduced:** yes (the mechanism, at the OS level, against a throwaway process)
- **Status:** fixed in 62c9edf

### N-3: manifest digests become cache paths unchecked
- **Where:** `registry.rs:151`, `registry.rs:187-193`
- **Scenario:** `blob` reads `cached(&d.digest)` with a digest taken from the manifest; `path` only replaces `:`. A tenant-signed manifest with digest `sha256:../../<path>` makes the supervisor read any file it can (or `/dev/zero`) and hand it to the tenant's worker. Cached reads also skip the hash check.
- **Reproduced:** no (from source)
- **Status:** fixed in bbeb543

### N-4: registry downloads have no size limit
- **Where:** `registry.rs:139-145`, `registry.rs:154-156`, `registry.rs:168-173`
- **Scenario:** The manifest, blobs, referrer manifests and Sigstore bundles are buffered whole in supervisor memory; referrers and bundles are fetched before verification, from a repository the tenant controls. One deploy can exhaust the supervisor's memory.
- **Reproduced:** no (from source)
- **Status:** fixed in bbeb543 (size caps) and db99779 (signatures counted per bundle); the overall deadline per fetch came with N-22

### N-5: a failed `rmdir` is still journaled
- **Where:** `quota.rs:476-478`, `journal.rs:186-191`, `journal.rs:267`
- **Scenario:** `RemovedDir` is recorded before the removal, without checking that an empty directory is there. When the removal fails, rollback creates a directory that never existed and that the file count never charged; repeated, it takes the volume past `max_files`.
- **Reproduced:** yes (journal compiled alone in a scratch harness)
- **Status:** fixed in 5f79152

### N-6: an inode is marked as size-recorded before the record is written
- **Where:** `journal.rs:155-158`
- **Scenario:** `sized.insert(ino)` runs before `record(Undo::Size(..))`. When the record fails (journal full), the retry skips it, and rollback can't undo the file's growth.
- **Reproduced:** yes (scratch harness)
- **Status:** fixed in 5f79152

### N-7: undo bytes are read through the guest's own descriptor
- **Where:** `journal.rs:162`
- **Scenario:** Overwriting a file opened write-only fails with `EBADF`, because the old bytes are read with `read_exact_at` on the guest's descriptor.
- **Reproduced:** yes (scratch harness)
- **Status:** fixed in 5f79152

### N-8: truncating an open file after unlinking it credits its bytes twice
- **Where:** `quota.rs:588-592`, `journal.rs:108-115`, `quota.rs:123-133`
- **Scenario:** Unlinking moves the file into the journal and credits its bytes. `local_file` still accepts the file there, so `set_size` credits them again. Repeated, committed data exceeds `disk_mb`.
- **Reproduced:** no (from source)
- **Status:** fixed in 60c46ed

### N-9: a failed `wasi:keyvalue` set keeps its quota adjustment
- **Where:** `kv.rs:118-123`
- **Scenario:** `reserve` changes the counters, then the journal steps can fail with a non-trap error (journal full); the reservation is never undone and the counters drift from the volume.
- **Reproduced:** no (from source)
- **Status:** fixed in 734468c

### N-10: eviction can drop an object that still has queued work
- **Where:** `table.rs:195-207`, `table.rs:150-151`
- **Scenario:** `evict_lru` picks entries whose phase is `Idle`, which an entry still is with a job queued but not yet received, or a WebSocket event waiting in the spawned send. The evicted actor drains its queue while the next request starts a second actor for the same object on the same volume.
- **Reproduced:** no (from source)
- **Status:** fixed in f98cb9a

### N-11: cancelling a version switch mid-drain can leave two workers on one app's volumes
- **Where:** `supervisor.rs:334-337`
- **Scenario:** The old `Live` is taken out of `w.live`, then the drain is awaited. If the request future running `start` is dropped meanwhile, the old worker keeps serving in-flight bodies while the next request starts a new worker.
- **Reproduced:** no (from source)
- **Status:** fixed in bbeb543

### N-12: a WebSocket path with two leading slashes is registered to the wrong object
- **Where:** `supervisor.rs:174`, `supervisor.rs:205`
- **Scenario:** The app is found after trimming every leading `/`, the object with `split('/').nth(2)` untrimmed: for `//counter/alice` the object is `counter`.
- **Reproduced:** no (from source)
- **Status:** fixed in bbeb543

### N-13: a worker can open any number of links to the supervisor
- **Where:** `link.rs:191-195`
- **Scenario:** Every fd sent on a control socket becomes a served connection, with no limit. A worker can make the supervisor hold any number of connections (each one a pending outbound request), and the worker-side outbound limit (`egress.rs:140`) is only enforced inside the worker.
- **Reproduced:** no (from source)
- **Status:** fixed in e410377; the cap is per worker and large (5,000 links by default), with nothing across the node: see N-27

### N-14: publish bodies are collected with no deadline
- **Where:** `ws.rs:407`
- **Scenario:** The supervisor collects a publish body up to `response_max` with no time limit; a worker that sends part of one holds that memory for as long as it likes, per link (see N-13).
- **Reproduced:** no (from source)
- **Status:** fixed in e62e9ac: proxy publish collection and supervisor forwarding have 30 s deadlines. N-13 (e410377) previously bounded memory per link, but not duration.

### N-15: forwarded responses have no body timeout
- **Where:** `egress.rs:281-285`, `egress.rs:367-369`
- **Scenario:** `serve_forwarded` bounds connecting and the first byte, but nothing bounds the response body after it, so a slow upstream holds the supervisor's connection and link for as long as it trickles.
- **Reproduced:** no (from source)
- **Status:** bounded by N-13 (e410377): a trickling body still holds its link, but the number of links a worker holds is now capped

### N-16: worker stderr without newlines grows one buffer without limit
- **Where:** `supervisor.rs:402-407`
- **Scenario:** The supervisor reads each worker's stderr with `lines()`; a worker that never writes a newline grows the line buffer until the supervisor runs out of memory.
- **Reproduced:** no (from source)
- **Status:** fixed in bbeb543 (lines capped at 4 KiB); no test, which needs a worker that misbehaves on purpose

### N-17: a warm worker is reused without checking the request's trust policy
- **Where:** `supervisor.rs:265`
- **Scenario:** The running worker is matched by digest alone, so a request naming the same digest with a different `nanovm-trust` or a higher `nanovm-min-version` is served unchecked. Matters only if the proxy sends different policies for one app name.
- **Reproduced:** no (from source)
- **Status:** fixed in bbeb543

### N-18 (efficiency): the registry copies every blob it stores
- **Where:** `registry.rs:206`
- **Scenario:** `store` returns `bytes.to_vec()`, a second copy of the component on every cache miss.
- **Reproduced:** no (from source)
- **Status:** fixed in bbeb543

### N-19 (efficiency): registry cache I/O runs on the front-end runtime
- **Where:** `registry.rs:191-207`
- **Scenario:** `cached` and `store` use blocking `std::fs` on the single-threaded front-end runtime, stalling every request on that thread while a component is read or written.
- **Reproduced:** no (from source)
- **Status:** fixed in bbeb543

### N-20 (efficiency): WebSocket fan-out scans every socket
- **Where:** `ws.rs:409`
- **Scenario:** Each publish walks all of the worker's sockets to find its object's; cost grows with the app's sockets, not the object's.
- **Reproduced:** no (from source)
- **Status:** fixed in bb8a418: the proxy's single socket tree is keyed by object and connection id; fan-out visits only that object's sockets, without a second index.

### N-21: jailed workers can't use a relative `data_dir`, so the demo fails
- **Where:** `object.rs:287`, `jail.rs` (the seccomp allowlist)
- **Scenario:** `new_store` canonicalizes the object's directory, and with a relative `data_dir` (the demo's `../nanovm-data`) that reads the cwd. `getcwd` isn't on the worker's allowlist, so every instantiation fails with `ENOSYS` and the demo answers `503 instantiate-failed` to everything. The tests use absolute paths, so they pass. Found running the demo while fixing N-2.
- **Reproduced:** yes (`demo/demo.sh`, and strace showing the refused `getcwd`)
- **Status:** fixed in d7f7ee6

### N-22: nothing bounds how many apps a node runs (high; blocks a public proxy)
- **Where:** `supervisor.rs:154` (the worker map), `registry.rs` (`Registry::load`)
- **Scenario:** since phase 7 the public names apps: any domain's TXT record, and with apps keyed by host, every host is an app of its own. Each new app gets a worker process, a uid, directories and a cgroup, and nothing caps them or removes them; worker-map entries (and their failure records) are never dropped. Loads aren't limited across apps either, and `read_timeout` is per read, so a registry trickling a byte every 29 s holds a load (up to `max_component_mb` of memory) for as long as it likes. A wildcard DNS record makes this free.
- **Reproduced:** no (found by reading, in the phase 7 review)
- **Fix:** a node `max_apps`, with idle workers exiting and leaving the map (else the cap is a permanent denial of service); a node-wide semaphore around `Registry::load`; an overall deadline per load.
- **Status:** fixed in add9b68

### N-23: a dev-mode node behind the proxy is reachable from the public
- **Where:** `config.rs` (`dev_on_loopback`)
- **Scenario:** dev mode requires a loopback `listen`, but a node behind the proxy listens on loopback anyway (the proxy's default `node` is `127.0.0.1:8080`). A dev-mode node there runs unsigned apps with private egress for anyone the proxy routes.
- **Reproduced:** no (phase 7 review)
- **Fix:** refuse dev mode in release builds (`cfg!(debug_assertions)` or a cargo feature), or document that it holds only while nothing forwards to the node.
- **Status:** fixed in 80cd5b6: dev mode needs the `dev` cargo feature (off by default); tests enable it through a self dev-dependency, the demo builds with `--features dev`

### N-24: the proxy (and the node) exit on an accept error, and the proxy has no handshake or header timeouts
- **Where:** `proxy.rs` (`serve`: `listener.accept().await?`, `tls.accept`), `server.rs:30`
- **Scenario:** at the fd limit, `accept` fails with EMFILE and `serve` returns, ending the process; about 1,024 idle TCP connections do it at a default soft limit. `tls.accept` has no timeout, and without a hyper timer there is no header-read timeout.
- **Reproduced:** no (phase 7 review)
- **Fix:** log, sleep about 10 ms and continue on accept errors; `timeout(10 s)` around the TLS accept; a `TokioTimer` and a header-read timeout on the proxy's connections.
- **Status:** fixed in 087db11 (the node's front end shares the accept fix; its test drives the proxy)

### N-25: the proxy's caches are unbounded and not single-flight
- **Where:** `proxy.rs:48-49` (`routes`, `digests`), `route`, `digest`
- **Scenario:** every distinct `Host` costs a DNS-over-HTTPS query and an entry that is never removed (a wildcard record makes it free); errors aren't cached, so a bad record is asked about on every request, inviting the resolver to rate-limit the proxy; a popular host's expiry sends a burst of identical queries.
- **Reproduced:** no (phase 7 review)
- **Fix:** drop expired entries past a size limit when inserting; cache errors briefly; one lookup in flight per host.
- **Status:** fixed in 3c8b418

### N-26: a running app's accepted policies grow without bound
- **Where:** `supervisor.rs:101`, `supervisor.rs:213-218` (`Live.policies`)
- **Scenario:** each distinct `(trust, min)` a request names is re-verified over the network (holding the app's start lock) and kept once accepted; varying `min=` or adding junk `trust=` entries grows the set and costs a referrers fetch each time.
- **Reproduced:** no (phase 7 review)
- **Fix:** clear the set past about 16 entries.
- **Status:** fixed in 0571c92

### N-27: links are capped per worker only, and high
- **Where:** `link.rs:192-201`, `supervisor.rs:328`
- **Scenario:** the N-13 cap is `max_live_objects × (max_outbound_concurrency + 1)` per worker, 5,000 by default, with nothing across the node. A worker that got out of Wasm can hold 5,000 to 10,000 supervisor fds (links plus upstreams); past the supervisor's fd limit, its accept loop fails and the node exits (N-24).
- **Reproduced:** no (phase 7 review)
- **Fix:** a node-wide semaphore shared by every `link::serve`, sized from the fd limit, or a much smaller per-worker cap.
- **Status:** fixed in c23cd12

### N-28: a drain doesn't wait for the old worker to be gone
- **Where:** `supervisor.rs:519-525` (`drain`)
- **Scenario:** `drain` drops the kill sender but doesn't wait for the process to exit, so the next worker can start on the same volumes a moment before the old one is dead.
- **Reproduced:** no (phase 7 review; predates phase 7)
- **Status:** fixed in 04d2e62; no test, since SIGKILL ends the old process far faster than a new worker starts, so the race can't be made to fail on demand

### N-29 (efficiency): uid lookup and chown run on the front-end thread
- **Where:** `supervisor.rs` (`uid`, `chown_tree`)
- **Scenario:** blocking filesystem walks on the single-threaded front end stall every request on that thread while a worker starts.
- **Reproduced:** no (phase 7 review; predates phase 7)
- **Status:** fixed in 04d2e62 (a latency change, no test)

### N-30: the proxy doesn't strip hop-by-hop headers
- **Where:** `proxy.rs` (`forward`)
- **Scenario:** `Connection`-listed and other hop-by-hop headers from the client are forwarded to the node. Low risk (the node is HTTP/1.1 and strips its own headers), but a proxy should drop them.
- **Reproduced:** no (phase 7 review)
- **Status:** fixed in ca4f51e

### N-31 (removable): code the phase 7 review found removable
- **What:** `check_digest` in `AppRef::from_headers` repeats the one in `fetch`, which runs before any path (1 line); TLS in the proxy (`proxy.rs` `tls`, `tls_cert`/`tls_key`, about 30 lines) if a terminator in front is assumed (owner's call: there is no ACME anyway); depending on `oci-spec` directly instead of oci-client, which the runtime now uses only for its re-exports (fewer dependencies, not lines).
- **Status:** partly done in 57d17a4: the runtime depends on `oci-spec` instead of oci-client (10 fewer crates in the runtime build; three small serde structs read the manifest fields it needs). The `check_digest` in `from_headers` stays on purpose: Reference parsing accepts upper case and other algorithms, and the early check answers a malformed digest with a 400 before any start lock or registry work. Its last part, dropping TLS from the proxy, is W-14 (decided).

### N-32 (tests): persistence tests coupled to small CPU budgets
- **W-9 baseline evidence (2026-11-08):** unchanged `kv_counts_against_the_disk_quota` fails repeatedly at 100 ms CPU (expected 198 small keys, then eight 1 MiB values in a targeted run); `kv_refuses_bad_names_and_streams_keys` hits its 10 s deadline in the full serial suite, then passes targeted. Same runtime passed CI 42437123623 on docs-only `fed57b1`. Production limits and fixtures are not changed in the storage experiment; this remains unresolved, not cured by a rerun.
- **SQLite fixture correction (2026-11-06, 14711ad):** main CI 42158465026 failed its setup write at the default 200 ms supervisor ceiling even though the fixture requested 2,000 ms. The same-runtime branch CI passed; the supervisor correctly clamped the tenant budget. Set the crash test's supervisor ceiling to its intended fixture budget. Production defaults and CPU-limit tests remain unchanged. Both P3/composed crash cases pass locally after correction.
- **Confirmed cause:** the registry version-drain test repeatedly failed because its unrelated multi-MiB quota workload exceeded the signed guest's 100 ms CPU budget. Its comparison of two HTTP clients' completion times also depended on client scheduling, not worker lifetime.
- **Registry fix:** persist a tiny file and KV value across the signed version switch; wait for observed request handling, require the admitted request to succeed, and check that the old process is gone. Warm both signed versions first: cold replacement loading could otherwise hide a missing drain. Disabling the actual drain loop fails with `worker-down`; restored cleanly. Final full default-scheduled and serial suites each pass (104 unit, 67 integration, one ignored). Quota tests and production budgets are unchanged.
- **Earlier quota fix:** d64c80a gave the dedicated in-process bulk-write fixtures an appropriate budget. CPU-limit tests remain unchanged.
- **Remaining uncertainty:** historical commit/journal failures did not reproduce in ten targeted repetitions or the full suites. No runtime fix for those is established. Do not treat a rerun as a fix, assume a network cause, or weaken CPU enforcement without evidence. Keep this residual investigation open; omit one-off incident narratives.

### N-33: initial proxy DNS lookups have no deadline
- **Where:** `proxy.rs`, `forward` → `resolve` → `lookup`
- **Scenario:** a resolver that accepts but never answers holds the initial HTTP request or WebSocket handshake before socket admission. W-3's event-delivery timeout does not cover that initial lookup. Predates W-3; found by its adversarial review.
- **Reproduced:** real stalled headers and JSON body both fail the new regression without the fix.
- **Status:** fixed in 225cfc4; a ten-second absolute route deadline covers cache waiting and resolver I/O. Full suite, mutation/restoration and demo pass.

### N-34 (tests): forged worker publish identity lacks a direct regression test
- **Where:** `supervisor.rs`, the egress link's publish branch
- **Scenario:** W-3 constructs fresh publish headers and stamps the worker's app, ignoring any worker-supplied `nanovm-app`, including duplicates. The review found no runtime flaw, but the normal-guest isolation tests do not exercise a compromised worker forging those headers.
- **Status:** deferred: source-reviewed boundary is correct; a malicious-link harness is needed without changing guests. Do not weaken the supervisor stamp when refactoring.

### N-35: persistent object directories exhaust filesystem link limits
- **Where:** `object.rs`, `new_store`; `demo/demo.sh` creates a fresh set of 5,000 object IDs each run
- **Scenario:** every persistent object volume is a direct child of the app directory. Repeated demos reached 65,000 directory links on this orb; new objects fail with `instantiate-failed` while existing objects work, despite free disk/inodes and live-store capacity. Eviction does not delete persistent volumes.
- **Reproduced:** yes: `stat` reports 65,000 links on the counter app directory and a direct `mkdir` fails with `Too many links`. The first affected demo had seven cold-start failures; the next had 65,000 instantiation failures. Existing solo/canary objects still answered. This predates the telemetry response changes.
- **Status:** open; plan directory sharding/data lifecycle without deleting existing volumes. W-2 validation uses a fresh disposable data root and preserves the exhausted directory.

### N-36: jailed-worker telemetry drops spans under sustained HTTP load
- **Where:** worker SDK export queue and supervisor relay; fixed by W-2, delivered on main in `49d0ae4`.
- **Reproduced:** oha 1.16.0 sent three 200,000-request runs through the real supervisor/jailed-worker path to OpenTelemetry Collector 0.164.0. All HTTP requests succeeded at 35–39k/s, but only 406,006/600,000 request spans reached the Collector. Its accepted/sent counts matched and refusal/failure counts were zero; the worker reported a full SDK queue. Even a 20k/s rate-limited run lost 2,439/200,000 spans.
- **Experiments:** 512-span batches reduced loss but did not eliminate it; two front-end threads also still lost spans. The SDK's experimental async processor with two concurrent exports delivered all 600,000 at 28–33k/s, but failed the strengthened flush barrier test. These experiments are not retained; do not claim the earlier 75–79k/s in-process result establishes production-path capacity.
- **Latency investigation:** mean worker export round trip 14.86ms, of which stamping was 0.68ms; Collector HTTP await was 9.40ms including scheduling. Lowering Tokio's I/O event interval did not help and was reverted. Stable concurrency 64 delivered 200,000/200,000 spans at 21,833 requests/s; concurrency 256 delivered 166,452/200,000 at 26,112/s. Next distinguish supervisor scheduling delay from Collector service time. These are session-specific observations, not a guaranteed capacity.
- **Scheduling fix:** the CLI now creates the supervisor on a dedicated one-thread control runtime; startup tasks use its captured Handle rather than the originating frontend. Relay/reaper/export I/O no longer competes with requests on frontend zero. Measured worker export round trips fell from 12.6 to 3.5 ms; the same front-thread-one workload that lost spans delivered all spans in three runs afterward. The frontend-lifetime regression fails when startup uses the old runtime. No custom exporter or queue/batch/sampling change.
- **Sustained validation:** 2,000,000 HTTP200 requests at 25,282/s, p99 28.6 ms, delivered 4,000,000/4,000,000 spans to official Collector 0.164.0 with derived metrics enabled; accepted/sent counts match and refusals are zero. Three preceding 200k runs also delivered all 400k spans each. This repairs the reproduced loss, not all overload/outage loss. The requested 64k HTTP target remains unresolved; historical main's confirmed 64k figure is in-process, while its HTTP rate is 27–29k on this orb. See HANDOFF for matched controls and CPU/timing evidence.
- **Status:** fixed in W-2, delivered in `49d0ae4`. Q71 accepts the separately budgeted Collector deployment; earlier measurements above are historical. Bounded best-effort export remains independent of request success; no lossless-accounting or universal capacity promise.

### N-37: final post-return commit failures lose diagnostic classification
- **Where:** `object.rs`, the final `commit(acc).await.is_err()` in `serve`
- **From source:** after sending a successful response, a failed final commit marks the request span as error but discards the I/O error and supplies no `error.type` or failing-phase classification. The initial commit has a `commit-failed` classification; the final one should be equally diagnosable without changing the already-sent HTTP status.
- **Status:** fixed in the focused W-2 continuation: retain the I/O result, record `post-return-commit-failed` and a bounded correlated diagnostic, preserve HTTP status. A gated test-only component outside `guests/**` proves actual post-return calls/logs and fails the second commit by moving its journal log. The regression failed before the fix; the full locked suite passes. Published guests remain unchanged.

### N-38: newly created app-directory ancestors were not synced
- **Where:** supervisor `app_dirs`; journal initialization synced through the app's objects directory, not its ancestors.
- **From source:** the first accepted write persisted the object's own directory entry, but a machine failure could lose an unsynced ancestor created during app setup. Process-kill tests cannot establish this guarantee.
- **Status:** fixed in 0e33e52, approved for main: sync the full canonical directory ancestry before launching the jailed worker; propagate every failure. Fault cuts at every ancestor fail initialization, and ignoring errors fails the regression. This covers supervisor setup, not physical power-loss certification or embedded Node callers' directory preparation.

### N-39: obsolete touched paths could fail commit with ENOTDIR
- **Reproduced:** a failed creation of `n/m`, followed by creating regular file `n`, made commit fail while syncing obsolete `n/m`.
- **Status:** fixed in 0e33e52, approved for main: treat `NotADirectory` like `NotFound` for obsolete sync targets; all other failures propagate. Regression fails before the fix and verifies accepted bytes after reopening.

### N-40: worker reap errors and failed-startup slot lifetime
- **Where:** `supervisor.rs`, worker watcher and readiness failure path
- **From source:** the watcher ignores kill/wait errors, so watcher completion is not unconditional proof of reaping. Failed startup drops `Live` and releases its app slot before waiting for the watcher. Both limitations predate the one-channel refactor; the normal replacement path waits for reaping, now asserted immediately by its integration test.
- **Status:** open; propagate termination failure before replacement and retain the slot through failed-startup reaping. Requires deterministic process-error/lifetime tests; not a demonstrated overlapping writer in the current tests.

### N-41 (tests): lifecycle follow-through coverage
- **Where:** proxy cache, actor deadlines, internal calls and scratch reuse
- **Review gaps:** directly exercise cancelled DNS initializer promotion without extending another waiter's deadline, cumulative handling/post-return CPU charging, internal-call cancellation's span/permit/wait-graph cleanup, and detached directory-task lifetimes before scratch reuse. Source review found no new blocker; passing existing tests do not establish these specific oracles.
- **Status:** open. Post-return queue isolation, exact recovered state across two reopens and immediate worker-reap assertions are addressed by `runtime-ownership-hardening`.

### N-42: cooperating local nodes can open the same app storage
- **Reproduced:** two Nodes admitted the same app directory; a second admission could clean live scratch and later recover another owner's journal. A separate preparation regression also reproduced supervisor preparation proceeding while the directory was locked.
- **Status:** fixed in 8c94218. Standard directory locks guard App lifetime and supervisor directory preparation. Tests cover retained handles, failed admission cleanup, separate apps and real jailed-worker release after SIGKILL; preparation rejection also passes as root. Both removed-lock mutations fail and restored full suite/SQL/KV/demo pass. No distributed fencing, noncooperating-writer protection or atomic preparation-to-worker reservation is claimed.

### N-43 (tests): max-app admission assumes requests stay busy during cold startup
- **Where:** `integration.rs`, `a_node_runs_at_most_max_apps`
- **Observed:** branch CI 42313030490 returned 200 when the fixture expected a full-capacity refusal. It holds workers busy with 600/900 ms sleeps while loading another app, then assumes they are still busy. Runtime sources are unchanged by backup/restore.
- **Status:** fixed on main through `1e5f18ad`, with passing CI: observed upstream requests stay gated until the capacity assertion completes. An injected 1100 ms delay reproduces the old failure and passes with the fix. Allowing eviction of busy workers fails the corrected regression; restored with `git checkout --` and empty unstaged diff verified. Restored full suite passes (99 unit, 69 integration, 12 ignored). Runtime and guests unchanged. Recovery's main CI 42313034048 passed independently; this fixes the race rather than relying on that rerun-equivalent success.

## Planned work

Agreed with the owner on 2026-10-30; PLAN.md's "Roadmap" has the order and the reasons, HANDOFF.md the decisions (Q41–Q51).

### W-1: per-object SQLite demo (priority)
- **What:** a guest where every object is its own SQLite database on `/data`, committed with its response, and a demo that shows it (for example a room or a ledger per object, with a crash mid-request rolled back). The showcase for nanovm.
- **Where:** `guests/` (a new guest), `demo/`, README. Adding or changing guests triggers `publish-guests.yml` on push to `main`, which publishes new versions: ask the owner first.
- **Experiment/proposal:** [SQLite over the existing filesystem](experiments/sqlite-filesystem/README.md). Real SQLite in Wasm with a P3 VFS works without runtime changes; 483 VFS cuts across DELETE/TRUNCATE/PERSIST/MEMORY recover SQL, files and KV together, as does a jailed worker SIGKILL. Composition now runs with upstream `spin:sqlite/sqlite@3.1.0` async methods: 3.0's synchronous calls cannot suspend for P3 I/O. Separate client/provider components pass typed results, resource drop/reopen, policy denial, quota refusal, mixed rollback, isolation and restart checks.
- **Journal follow-up:** the original runtime retained 120 deleted SQLite journals (1,046,400 bytes) and logged 991,232 bytes covering only 8,192 distinct database bytes; the retained-file cap was reached first. Independent generic fixes deduplicate original-byte backups and discard request-created files with live-handle accounting. Combined validation completes 1,000 updates per mode: DELETE uses a 56,662-byte undo log, the other rollback modes about 8.7 KiB, and all retain only the 6-byte original KV marker. Commit/rollback, capacity failures, the 483 cuts and worker SIGKILL pass. Generic fixes and the rename-failure correction landed on `main` through [40c5895](https://github.com/carsonfarmer/nanovm/commit/40c58953f49919f1315052b5d25c4e0836ff63cc); the SQLite experiment remains separate.
- **WAL investigation:** exclusive-locking WAL works over the unchanged P3 VFS, with no shared-memory file: set locking mode before first access. Added 92 mutation cuts and 114 checkpoint cuts (PASSIVE/FULL/RESTART/TRUNCATE), bringing the total to 689. The default checkpoint threshold exhausts the 1 MiB volume; `wal_autocheckpoint=16` completes 1,000 updates in this fixture with exact commit/rollback. This establishes single-connection WAL only, not multi-connection locking.
- **Benchmark/proposal:** 2,700 timed requests across six workloads, cached/reopened connections and three journal policies; [method, raw data and results](experiments/sqlite-filesystem/README.md#delete-versus-exclusive-wal-benchmark). WAL helps cached writes but checkpoint policy changes the winner; reopening loses most benefits and slows reads. Simplified contract: one owned connection, internal DELETE/FULL, no app-facing journaling controls, and no raw SQL transaction control. Multi-connection WAL is out of scope. KV/config Wasm-provider exploration is W-18.
- **Provider hardening:** bounded inputs and aggregate outstanding results, cancellation cleanup and drop/reopen accounting pass. Storage failures and write-result overflow trap to force whole-request rollback; ordinary SQL errors remain catchable. Concurrent async calls serialize with the current blocking P3 bridge. The composed provider passes 114 additional VFS cuts and a jailed-worker SIGKILL after SQL commit, recovering SQL/file/KV together. No runtime changes.
- **Further hardening:** invalid UTF-8 in write RETURNING results now forces rollback; previously catching the conversion error could commit unseen writes. Read-only conversion failures remain catchable. CPU exhaustion inside SQL and timeout during a test-injected host wait also recover SQL/file/KV together. A caller-side cancellation probe completed on its first poll and supplied no mid-call cancellation evidence; that gap remains explicit.
- **Beta blocker addressed locally:** recovery used to discard failed undos, and a failed commit could be retried as a false success. Generic fixes now preserve failed recovery records, poison failed commits, and sync both parents of reversed renames. Real filesystem obstruction regressions and 38 injected sync errors pass; the composed provider recovers after two obstructed startup attempts. These changes must land before promotion.
- **Beta scope:** one composed provider/connection per object, memory-only temporary SQL storage, no direct access to its database/sidecars, no individual in-flight query cancellation, and local storage honoring syncs. These exclusions need documentation, not new host APIs. Commit/transport errors can have unknown outcomes; apps need idempotent retries. See the [beta contract](experiments/sqlite-filesystem/README.md#scope-for-a-production-beta). Physical power-loss certification, broader host-fault injection and hard isolation from same-app raw-file access remain unproven; this is not a general-purpose VFS or published interface.
- **Source beta:** [components/sqlite](components/sqlite/README.md) packages the provider and a small HTTP app in a locked workspace. `cargo run --locked --example sqlite` checks persistence, isolation, mixed SQL/file/KV rollback and restart on a real in-process Node. CI builds the normal provider, runs that example and tests the same source with the pressure fixtures. No new host API or journal design; no publishing workflow is enabled.
- **Query cancellation — closed without implementation:** blocking `thread.yield` is intentionally non-cancellable under the component-model specification, not a Wasmtime bug. Query-only cancellation remains outside the beta contract; whole-request interruption and rollback remain supported. Diagnostic probes were removed. No runtime fork, new host API or scheduling redesign is planned; see the [decision](experiments/sqlite-filesystem/README.md#query-cancellation-investigation-2026-11-02).
- **Status:** source beta/example and host recovery fixes are on main (through `0baff082`); main CI passes the example/pressure checks. SQLite has not been published as a package. Counter/hostile migrated packages 1.0.11 were published separately with owner approval.

### W-2: OpenTelemetry by default; deprecate the stats and events APIs (high priority)

**Delivery:** done in `49d0ae4bf6e3d336fab8415f68aa7faa15782c81` on main. Main CI 41543687359 and branch CI 41543685753 passed. No production deployment, guest changes or package publishing. The owner-requested deep simplification analysis and N-32 test fix are on `runtime-simplification`, based on the unmerged `w2-docs-simplification` follow-up. Preserve behavior, safety bounds and performance.

**Simplification analysis:** prior egress cleanup reduced runtime 5,315 → 5,305. The owner rejected the initial incremental estimate. Revised proposals change ownership boundaries: actor-owned lifecycle phases, storage-owned complete mutations, a common dependency-call lifecycle, and stock WASI registration with selective mutation overrides. See PLAN for upstream findings and verification gates; no new LOC ceiling or implemented savings are claimed. Workspace Rust and grilling skills are installed. The reproduced registry test defect is fixed as described in N-32; follow-up remains unmerged and makes no new performance claim.

**Complete (Q71):** owner delegates deployment to engineering. Recommend a separately budgeted standalone/gateway Collector, or dedicated physical cores on the same host. The measured 2.17% application overhead satisfies that deployment's gate; 7.58% colocated overhead and 13.62–14.70 µs/request additional Collector CPU remain explicit costs. Runtime 5,315 versus baseline 4,955 (+360, 7.3%). Correctness, complete exports, full suites, mutations, production check, demo, quick benchmark and CI pass. No guest changes or LOC-only refactor. Remote-machine and durable-backend performance are not tested; operators must validate their backend/topology. Earlier pending-decision checkpoints below are superseded; proceed with validated fast-forward delivery.

<details>
<summary>Historical W-2 checkpoints — superseded by the delivery record above</summary>

**Current delivery/acceptance:** implementation and validation preserved on `w2-observability`, not merged. Post-commit controls at `9b8f8fb`: main/W-2 18,631/18,227 requests/s with equal application physical-core allocation and separate Collector/load-generator cores (2.17% lower); 27,548/25,459 fully colocated (7.58% lower). Active application CPU 103.45/105.00 µs/request isolated, 109.42/110.64 colocated; Collector adds 14.70/13.62 respectively. All 2.4m spans in each matrix and all 4m in a sustained 2m-request run arrived. Long-path/tracestate/overload exports also complete. Full locked suite passed twice (104 unit, 67 integration, one ignored), production check/demo/quick benchmark/mutations pass. Owner decision on separate Collector CPU budgeting remains; do not infer acceptance of the colocated cost. No span consolidation, sampling or guest changes. HANDOFF records the executable recipe and both favorable and unfavorable earlier results.

**Final optimization / Q70:** owner requires all meaningful supervisor/worker/proxy/dependency spans; the consolidation proposal below is rejected. Native profiling identified repeated pure OCI parsing. A bounded per-thread cache (16 owned inputs ≤1024 bytes) leaves all per-request trust/host/version/digest checks intact; common span attributes avoid reallocation. Three matched 400k-request pairs measure 0.74% lower aggregate throughput than main with Collector physically isolated, 4.45% lower fully colocated. Every expected span arrived (2.4m per matrix), with all HTTP200 responses. Cache-bound/key mutations were caught, full locked suite and production check passed; demo passed 70k requests and contained 100 attacks with zero canary failures. Runtime 5,315 lines; no guest modifications. Post-commit repetition/final delivery remain. HANDOFF has exact measurements and retained prior evidence.

The checkpoints below are historical, not outstanding product decisions.

**Post-commit CPU isolation:** acceptance is within a few percent of matched main, not 64k over HTTP. On `1adadcc`, six warm 200k-request runs per variant with application and Collector on separate physical cores measured aggregate main 13,001 versus W-2 11,635 requests/s (10.5% lower). Application CPU 147.0→165.2 µs/request; Collector 23.45 µs/request separately. All 2.4m W-2 spans arrived for 1.2m HTTP 200 responses. Thus the remaining gap is not solely colocated Collector CPU. A diagnostic omission of supervisor-hop events improved 2.6–8.7% in earlier matched rounds but removes coverage and is not retained; batch1024 had inconsistent throughput and risks oversized exports. Independent node-ingress latency versus consolidating successful hop events is the remaining proposed product tradeoff. Preserve all coverage until decided; W-2 remains unmerged and its performance gate open. HANDOFF records CPU topology, raw results and reproduction.

- **Latest continuation:** post-return and front-door coverage, control-runtime isolation and an official Collector reference pipeline are implemented. Full locked suite, mutation checks, production build, demo and quick benchmark pass. Real Collector-derived request/error/histogram counts are verified; a sustained 2m-request HTTP run exports all 4m spans. The remembered ~64k main benchmark is reproduced in-process; full-export W-2 is 75–78k there, not over HTTP. Final alternating clean-build HTTP controls are main 23.8–25.2k versus W-2 17.2–23.3k, with all spans delivered. Do not discard the slower runs or claim the performance gate is cleared; branch unmerged.
- **Focused continuation:** N-37 and actual post-return call/log verification are implemented. Proxy/supervisor request events cover pre-worker rejections and propagate context into the worker. Full locked suite passes (104 unit, 67 integration, one ignored). N-36, mutation/review follow-through, production validation, demo and benchmark remain; no merge. The owner rejects a ~22k complete-export operating envelope as the resolution; measure matched HTTP throughput and full export against the requested ~64k target.
- **Review checkpoint (Q69):** cancellation before headers now marks dependency spans as cancelled; external error classifications ignore upstream private headers; runtime diagnostics carry explicit request context. Regressions failed before each fix. User requested stopping at this checkpoint. N-37, actual post-return call/log attribution, pre-worker coverage and N-36 remain; do not merge or mark W-2 complete.
- **External HTTP coverage:** CLIENT spans and SDK W3C propagation now cover external guest requests, including denied calls and handled 4xx/5xx responses. Queries are excluded from spans and denial logs. Local-upstream regression failed before implementation and checks actual received context plus decoded OTLP. Pre-worker coverage, final review and N-36 remain unfinished.
- **Correctness first (Q68):** owner suspended LOC enforcement and asked for Honeycomb-guided instrumentation before optimization. Main's journal changes through `40c5895` are integrated. Inter-object spans now record CLIENT kind, target, method/path, status and errors; request spans use SERVER kind. Regression covers success, callee trap, cycle rejection and 404 with concurrent independent traces. Remaining coverage: external HTTP client spans/context injection, pre-worker proxy/supervisor failures, post-return/background attribution and boundary privacy. Preserve standard SDK batching and configurable Collector; N-36 remains open.
- **Production-path validation:** N-36 records the real-Collector gap. The existing jailed-worker test now checks four concurrent requests queued on one caller, each retaining its own caller → call → callee trace parentage. The queue test also requires flush to wait for in-flight exports; it rejects the concurrent SDK experiment. Runtime remains 4,993 lines; no experimental runtime or dependency changes are retained.
- **Allocator/export milestone (Q66):** Linux CLI/benchmark use library jemalloc; embedders keep allocator choice. Runtime 4,993 lines, no new instrumentation framework. A deterministic stalled-export test failed with the old queue (1280/1792 delivered); a 2048-span queue retains that burst while dropping overload at its bound. `bench --export` checks decoded spans and drain time: three runs received 601,000/601,000 at 75–79k/s. Full suite passes (94 unit, 65 integration, one ignored). Remaining: production-path throughput, main comparison, concurrency/parentage coverage and adversarial review; no merge yet.
- **Log-path simplification:** 4,999 → 4,990 runtime lines by removing redundant log/event wrappers; guest correlation, field names and body bounds are unchanged. Allocator and full-export burst experiments continue; W-2 remains unmerged.
- **Runtime budget / response milestone (Q65):** 5,016 → 4,999 runtime lines with the unchanged count. Preserve trace detail and snapshot fields; simplify SDK conversion, response plumbing and snapshot ownership. Direct and worker-link transports now return bounded Collector status/body to the SDK, whose existing parser reports partial rejection through stderr diagnostics. Validation is recorded in HANDOFF; W-2's throughput, further concurrency/parentage coverage and review gates remain open.
- **Current owner-approved migration (Q62):** in progress by Amp on `w2-observability`. Always-on event-oriented instrumentation replaces the tracing feature and metric exporters: rich request spans, correlated logs, periodic infrastructure snapshot events, configurable OTLP collector. Phase detail stays on the request; analytics and sampling belong downstream. The earlier metric/feature milestones below are historical. Guest API work is W-17.
- **Event-model milestone (Q63):** implemented with OpenTelemetry SDK spans/logs/batching, no custom event framework. One actor-owned request span survives post-return commit; phase fields and exact guest-log correlation are tested, including unsampled incoming parents and restart context. Metric exporters/local histograms and the tracing feature are removed. Full serial suite passed (92 unit, 65 integration, one ignored). Matched long probes did not demonstrate a throughput win: preceding W-2 median 65,604/s versus events 64,309/s; main comparison 84,518/s versus events 64,245/s. No merge; final mutation/demo/benchmark follow-through and remaining W-2 correctness/review gates are recorded in HANDOFF.
- **Complete-export measurement (Q64):** the owner accepts roughly 64k/s with full observability and prioritizes simplicity. An active local decoding sink exposed 37–38% dropped request events with 64-span SDK batches. Batches of 256 (same queue/body caps) and stamping only at the worker trust boundary delivered all 601,001 events repeatedly, mostly at 50–54k/s. Keep the library-based simplification, but do not call the no-listener 64k result full observability. Further throughput work remains; do not increase complexity merely to chase noisy orb numbers.
- **What:** OTel traces (a span per request, with queue wait, instantiation, guest time, commit, calls), metrics (what `/_nanovm/stats` reports today) and guest logs (each object's stdout and stderr), on by default and tagged with app, object and version. Then remove `/_nanovm/stats` and `/_nanovm/events`.
- **Design notes:** a jailed worker can't open sockets, so its telemetry has to leave through the supervisor, over the egress link or its stderr; the supervisor exports OTLP to an operator-configured collector. Look at the `opentelemetry` and `tracing-opentelemetry` crates before designing. Touches the supervisor, so coordinate with the rewrite.
- **Where:** `metrics.rs`, `object.rs`, `server.rs`, `supervisor.rs`, `demo/demo.sh` (it reads `/_nanovm/stats`).
- **Status:** in progress by Amp on `w2-observability` (2026-10-31), incorporating main's enabled CI. Readiness is independent of telemetry (Q57). Bounded SDK/OTLP transport, supervisor identity stamping, request spans, worker counters and guest logs are implemented (Q58–Q60); real jailed-worker exports and adversarial relay tests pass. Consumer migration/API removal is complete. Remaining: collector error reporting, further hostile load/trace-parentage coverage, review and resolution of the performance gate. HANDOFF records the checkpoint and successor prompt. No guest changes.
- **Landing blocker:** transport CI/full suite/demo pass, but default full-trace warm GET throughput regresses. Removing synchronized span wrappers improves the measured 36,897/s to 44,546/s, still below main's 72,863/s on this orb. Sampling and phase-event experiments were not retained. HANDOFF records seven caught mutations and benchmark caveats. Do not mark W-2 done or merge until the performance requirement is resolved.
- **Performance investigation / Q61:** repeated 200,000-request warm probes isolate span recording and batch handoff as the main overhead, not metrics alone. Five spans are recorded per ordinary warm request (including both commits). The owner requested an optional compile-time switch: `tracing` is default-on; `--no-default-features` retains metrics/logs without recording spans. Full tracing remains unchanged in detail, and its performance blocker is not waived. Async SDK batching, shared object strings, larger batches and boxed spans did not improve the repeated comparisons and were reverted.
- **Retained optimization:** preallocate room for the request span's response status instead of reallocating at completion. Alternating warm comparisons improved about 50k→54k requests/s with full detail; no sampling change. Trace-free builds measured 63–68k/s. Remaining full-tracing overhead is still a landing blocker.
- **Worker coverage:** runtime event logs, instantiation histograms and phase gauges now export and pass the real jailed-worker test. Supervisor worker state, starts/link errors, RSS and uptime also export. Tests and demo consume OTLP; stats/events APIs, event ring and JSON serializers are removed. Shutdown exports pending signals off the async runtime thread. A 4,096-object regression caught batches exceeding 1 MiB at the original 2,048-series cap: Q60 reduces it to 512 plus overflow while preserving totals. Collector response/error reporting, stronger concurrency/trace coverage and final review/performance validation remain outstanding. Latest full suite: 94 unit, 65 integration passed, one ignored. Latest quick warm GET: 41,849/s, still below the baseline.

</details>

### W-3: move WebSockets into the front proxy
- **What:** the proxy holds client sockets, Pushpin-style, and sends each event to the object as the plain `POST` it gets today; publishes go to the proxy. Most of `ws.rs` leaves the node, and the node only answers requests. Resolves N-14 and N-20. Supersedes Q31.
- **Where:** `proxy.rs`, `ws.rs`, `supervisor.rs`, `egress.rs` (`hold`), `object.rs` (`commit`), the WebSocket tests.
- **Status:** done in e62e9ac (migration), bb8a418 (cleanup). Runtime 4,247 baseline → 4,264 implementation → 4,246 cleanup. Full suite passed twice: 93 unit, 64 integration, one ignored; seven mutations caught and restored cleanly. Demo: 70,000 normal requests succeeded, 100 attacks contained, canary zero failures. Quick benchmark has mixed small variation and unchanged 96 KB/object density; HANDOFF records numbers and limitations. Adversarial review found no confirmed runtime blocker; identity test gaps fixed, N-33/N-34 recorded. Guests unchanged.

### W-4: alarms, outside the node
- **What:** a scheduler beside the proxy. An object sets an alarm with a response header; when it fires, the scheduler sends the object a request. Durable across restarts of the scheduler; the node is unchanged.
- **Status:** open, soon after W-3

### W-5: data lifecycle (planning)
- **What:** a plan for deleting an app's or an object's data, exporting a volume, backups and retention, and how each is authorized (likely through the proxy).
- **Status:** offline backup/restore done in `fab908c` with validated main follow-through. Restic snapshots retain journals under app admission locks; only complete snapshots are tagged, and restore requires an exact ID and new destination. Real jailed SQL/file/KV crash recovery into a fresh root passes unprivileged and as root, including another write/restart. Six mutations caught; full suite, ten SQL tests, Python CLI tests, demo, benchmark and adversarial review pass. Host runtime unchanged at 5,085; tooling adds 89 lines. README defines offline authorization, ownership/deployment records, retention guidance and old-writer fencing. Deletion API, live snapshots, automated retention and actual off-node deployment remain open; no active file ownership after main integration.

### W-6: JS and Python guests
- **What:** guests built with componentize-js and componentize-py, running the same endpoints as `counter`, to prove the host for other languages.
- **Status:** open

### W-7: `nanovm publish`
- **What:** one command that pushes an app with `oras` (layout as in `publish-guests.yml`) and signs it keylessly (`cosign` with a browser sign-in, or a CI attestation).
- **Status:** open

### W-8: benchmarks against workerd and Spin
- **What:** density (memory per object), cold starts and warm latency against workerd (Durable Objects) and Spin, on the same machine, with the method written down.
- **Status:** open

### W-9: ordered committed-state transfer and recovery
- **What:** capture immutable committed content and namespace state before later mutations, transfer ordered versions and recover without the original root. The undo journal is not a replication feed. Survey Litestream, LiteFS and Durable Objects before choosing machinery.
- **Status:** in progress, Amp on `w9-atomic-capture`, following main proof delivery. Owns journal/capture/config integration and its tests: opt-in, bounded local immutable capture tied to the existing commit decision, recovering pending publication before further writes. Local acknowledgement remains unchanged; remote durability and failover policy remain unresolved. Offline backup is complete, not this work's objective.
- **Evidence:** example tests and separate executable drill pass: ordered/idempotent receipt, immutable response/post-return versions, exact file/namespace recovery after source deletion, rejected mutation rollback, receiver process-exit cuts and continued local writes after recovery. Four guard-removal mutations fail and are restored. An unsent locally accepted write is deliberately absent on recovery. Same-host roots/processes do not establish remote durability; full local suite retains N-32 failures. Review found no blocker for the disclosed research scope, not production integration.
- **Delivery:** bounded proof done in `310c992`, CI 42438468359 passed the full suite and SQL/KV checks. W-9 stays open for crash-atomic source capture and runtime integration; no active file ownership after main integration.

### W-10: groundwork for many nodes
- **What:** a lease hook before instantiation in `table.rs` (always granted on one node), an epoch carried by each commit, request ids so the proxy can retry safely. Placement belongs in the proxy.
- **Status:** open, after W-9 capture/recovery evidence and explicit acknowledgement/fencing decisions; not authorized by adding an epoch field alone.

### W-11: run requests that don't write in parallel
- **What:** stateless routes on any node, and read-only requests on read replicas of an object. On one node, a reader beside the writer would see uncommitted data (the undo journal writes in place), so replicas need committed state: a snapshot, or W-9's change records. Whether a request writes isn't known in advance, so routes would be declared read-only, with `/data` mounted read-only to enforce it.
- **Status:** open (research)

### W-12: trusted caller header on calls between objects
- **What:** the node sets `nanovm-caller: <id>` on calls and strips it from client requests, so an object can serve internal endpoints only to its app's objects. Left out of the first version because testing it needs a guest that echoes headers (a guest change; see W-1's note on publishing).
- **Status:** open

### W-13: removable code in the rewrite's files
- **What:** from the review's list, for the agent rewriting these files: drop the registry's manifest cache and the SHA-256 re-check in `Registry::store` (oci-client already checks digests) and its `to_vec` copy (N-18); drop the `memory_mb <= memory_reservation_mb` checks in `config.rs` (the pool and the limiter already enforce both). Keep the digest-format check of N-3. Owner's call, smaller: stop forwarding the guest's connect and first-byte timeouts, the fs-denied event throttle, the worker-side `well_formed` check on publishes.
- **Status:** done: the config memory check is gone (f56887e). The registry half no longer applies: with our own registry client (phase 7) its hash check is the only digest check, so it stays, and the manifest cache went with db99779

### W-14: drop TLS from the proxy (owner decided 2026-10-30)
- **What:** remove `proxy::tls`, the TLS branch of `proxy::serve` (and its handshake timeout), `tls_cert`/`tls_key` in `ProxyConfig`, the `tokio-rustls` dependency if nothing else uses it, and the TLS half of `the_proxy_outlasts_idle_and_excess_connections`. TLS is left to a terminator in front (Caddy, a cloud load balancer, Cloudflare), which also handles ACME and certificate reloads that the proxy never had. It is in git history if wanted back. Update README's "The proxy".
- **Status:** fixed in f49077f

### W-15: key apps by the DNS name that owns their record (owner decided 2026-10-30)
- **What:** today an app is keyed by the request's host and repository, so `example.com` and `www.example.com` are two apps with separate data. Key it instead by the name whose `_nanovm` TXT record was found, after CNAMEs: the `name` of the TXT answer in the DNS-over-HTTPS JSON, lowercased, without a trailing dot or the `_nanovm.` prefix. The proxy sends that as `nanovm-host`. Then `_nanovm.www.example.com CNAME _nanovm.example.com` makes both hosts one app. Still safe: a CNAME to someone else's record only serves their app as their record configures it, which anyone could already reach through their own domain; no name can choose another name's version or data. Test with the DoH stub answering a CNAME chain (two hosts, one app; a third host with its own record, its own app). Update README's "The proxy" and "Apps", and Q52 in HANDOFF.
- **Status:** fixed in 54054c0

### W-16: build and test CI
- **What:** run a production-feature build check and the unit/integration suite on pushes and pull requests; build guests locally without publishing packages. Read-only token, Ubuntu 24.04, locked dependencies, serial test scheduling to reduce N-32 contention.
- **Status:** done in d6f89f9, runner fd-limit correction in 57d1595. GitHub run 41227208960 passed: production build check, 93 unit and 64 integration tests, one ignored. Workflow checks are enabled; no branch-protection rule was changed.

### W-17: guest instrumentation API
- **What:** let guests enrich their request event and create child spans using propagated context. Survey existing WASI/OpenTelemetry interfaces before introducing a custom API. The host owns app/object/version identity, bounded fields/events and the collector destination; guest input must not spoof identity or cross request/tenant boundaries.
- **Status:** follow-up requested by the owner; outside the W-2 event-oriented host migration. Guest changes require explicit approval before editing or publishing.

### W-18: explore key-value and config providers inside Wasm
- **Delivery approved (2026-11-06):** owner authorizes main integration and publication of the migrated counter/hostile guest packages. Runtime `0e33e52` passed branch CI. The main push publishes via `publish-guests`; older publication holds below are superseded. The page replacement remains rejected and the net LOC-reduction goal remains unmet.
- **Journal follow-through complete on branch (Q85):** full locked suite 97 unit/68 integration, all nine opt-in SQL tests, composed KV contracts and demo (70k normal requests, 100 attacks, 595 successful canary requests) pass. Lazy buckets make fresh read-only KV requests 3.7× faster; runtime warm throughput improves about 15% and idle memory 143 → 119 KiB/object. Writes remain essentially unchanged. Runtime ends 5,082 versus 5,074, so the requested net LOC reduction remains unmet. Review's counter-before-slot-release race is fixed; scratch-stream detached host-task lifetime and production physical-power-loss certification remain validation gaps, not demonstrated new leaks. N-38/N-39 are addressed on this branch. No main merge or package publication; inherited guest migration requires approval. See HANDOFF's successor prompt and experiment README for reproduction.
- **Journal follow-through (2026-11-05):** `journal-runtime-improvements` retains the journal and borrows bounded simplifications from the improvement branch, 5,074 → 5,021 runtime lines before performance work. Full suite 92 unit/67 integration passes (11 ignored); corrupting saved-range selection fails the existing recovery regression and is restored cleanly. Registry async-write/rename cleanup was skipped because cancellation can leave an intermediate file. Actor/mailbox and lookup performance follow-through continues; no adoption of the page backend or guest publication.
- **Decision Q84 (2026-11-05):** retain journaling; reject adoption of the current per-object redb replacement after optimizing and measuring plain pages too. Small writes improve ~3× and unique 1 MiB writes reach parity, but lifecycle throughput remains 3–5× worse, idle density 143 → 210 KiB/object and replacement LOC 5,074 → 5,368. Experimental branches remain reproducible; incomplete hardening is not claimed complete. Continue measured journal/runtime improvements from the other branch. [Full decision and successor prompt](experiments/fs-backends/README.md#adoption-decision-retain-journaling-2026-11-05). Earlier instructions to continue page integration below are historical.
- **Stream-lifetime follow-through (2026-11-05):** real component reader/writer/directory streams survive descriptor drop, unlink and acceptance; held entries remain charged until final stream destruction. Exact quota reclamation, later rejection and restart pass. Premature inode release fails the regression; restored with `git checkout --`, empty unstaged diff confirmed and restored run passes. Runtime unchanged; remaining gates below still apply, except stream-lifetime coverage is now complete.
- **Real adapter checkpoint (Q83, Amp, `hashed-fs-integration`, 2026-11-05):** hash pages now run behind temporary `page-fs`, with 88 unit/68 integration tests passing (11 optional tests ignored). Parent reproduced/fixed database teardown racing shutdown, missing jail OFD locks, and the review's missing database-name directory sync; the sync mutation fails and is restored cleanly. Actual capped-adapter locks and stable two-reopen recovery after post-return sync failure are covered. A redundant orphan table is removed. Default storage remains journal; no merge or guest changes.
- **Adoption remains blocked:** repeated-value KV PUT 128 B/1 MiB improves 2.817/7.282 → 0.911/5.217 ms, but unique 1 MiB PUT regresses **7.154 → 16.764 ms** over three additional pairs. One matched quick pair drops cold/churn throughput 2,775/4,298 → 958/769 per second and increases observed density 144 → 210 KiB/object; a final candidate rerun confirms those regressions. Replacement runtime estimate is 5,424 versus 5,074 (+350); provider unchanged. Finish real stream lifetime coverage, unchanged-cap runtime amplification, ancestor-directory durability and backend-neutral optional SQL oracles; improve actual complexity/startup costs before swapping. The composed SQL contract and both full suites pass (default after an N-32 setup CPU-budget failure/rerun). Demo passes 70k normal requests, contains 100 attacks, and serves 486 canary requests without failures. [Protocol and handoff](experiments/fs-backends/README.md#runtime-integration-checkpoint-2026-11-05).
- **Capacity follow-through (Q82, Amp, `hashed-fs-integration`):** six fixed-cap exhaustion cases retain the same database-file-length cap through recovery and two reopens, preserving committed data, orphan accounting and locks. Subsequent small mutations pass. Direct boundary tests cover overflow, no partial write on refusal, shrink-at-cap and lowered-limit refusal without truncation; removing growth enforcement fails. This is a file-length boundary, not a total-memory/allocated-block guarantee or proof that every exhausted database can reclaim space. The real hierarchical core and WASI adapter are now in progress; production swap and end-to-end gates remain open.
- **Hardening implemented (Q81, Amp, `hashed-fs-integration`):** orphan indexing removes quadratic namespace recovery and linear close scans. Real-file tests pass 684 mutation/commit and 134 orphan-recovery failure cuts, including partial writes and shared-content reclamation. Two orphan-index mutations fail and are restored. Review's repeated-reopen oracle gap is fixed, and the underlying commit outcome is checked separately. Full host suite passes; physical/total-memory bounds and WASI integration remain open. Production unchanged; experiment inode layer 228 → 238 lines, not a claimed runtime reduction.
- **Integration in progress (Q80, Amp, `hashed-fs-integration`):** owner requests real backend integration with LOC/performance/simplicity confirmation before merging/swapping. Source/history assessment of `cf/optimistic-feynman-62f1a0` at `86ab1dc` is in HANDOFF. Reuse measurement methods, lazy bucket creation and relevant filesystem regressions; do not combine scheduler/guest changes into the backend comparison or import the branch wholesale. No implementation changes in this assessment. Orphan-scan scaling, failed-storage behavior, WASI directories/streams, jail compatibility and matched end-to-end gates remain open.
- **Migration in progress (Q75, Amp, `spin-kv-prototype`):** owner authorizes guest/test migration and removal of native KV/old experiments. [Provider and contract](components/keyvalue/README.md). Env/config remain injected. Owner also invites filesystem improvements justified by stress/performance evidence. No merge or package publication yet.
- **Lifecycle fixes:** executable tests reproduced cancelled-write temporary orphans, candidate collisions, and listing slots released before completion delivery. All now pass: mutations defer cancellation through cleanup/publication, candidates skip existing entries, and slots cover completion lifetime. Full migrated suite passes (91 unit/67 integration), as do optional SQL/crash coverage and 10,000-key stress. Review's pool-capacity finding and stress's full-quota reopen finding are fixed on this branch with regressions.
- **Adoption blocker:** matched release KV reads/writes regress (1 MiB GET 1.413 → 3.155 ms, PUT 4.614 → 7.126 ms). Actual runtime including the provider saves only 13 lines, with additional build/CI wiring. Logical quotas do not cover block/metadata amplification: 10k three-byte files occupy 5.12 MB allocated on this orb. See provider README for method, complete results and next profiling work. N-35 remains a separate object-directory scale limitation. Do not mark W-18 production-ready or merge merely because correctness passes.
- **Profiling follow-up (Q77):** generated Wasmtime 50 byte-list adapters copy element by element. An isolated bulk-copy patch measures 1 MiB GET native/stock/patched 1.404/3.149/1.600 ms, PUT 4.925/7.531/5.854 ms; component probes pass, but upstream compiler edge-case tests remain. The patch is saved under experiments, not enabled. Sibling temporaries remove 602 directory fsyncs per benchmark; small-write wall-time improvement is unproven. Tentative-state rejection is the requirement, not undo logging; snapshots/COW and external committed-version replication remain options. Small-call/write overhead still blocks the unchanged acceptance gate.
- **KV-first scope (Q73):** owner keeps env/config host-injected for now. The config exploration below is deferred.
- **Inode/quota/cache proof (Q79, Amp, `fs-page-semantics`):** both variants share flat namespace and held-inode semantics, logical quotas and restart orphan cleanup. Owner favors hashes when timings are close. The 60-row bounded-cache matrix finds near ties in small writes/large reads at 1 MiB cache, but not unique large replacements. Six namespace SIGKILL checks pass. Not a WASI adapter: namespace-scan scaling, total-memory/physical-storage bounds, storage-error injection and mixed SQL/KV/raw-file integration remain gates. See the experiment README; production unchanged.
- **Bounded filesystem screen (Q78):** implemented on `fs-backend-comparison`; [protocol/results](experiments/fs-backends/README.md). Actual journal versus redb plain/hash-addressed pages, 108 storage-only measurements plus visibility/rejection/reopen and nine process-death checks. Plain pages improve small writes and batches but worsen warm reads and often write more bytes. Hashing benefits shared-content batches, not unique large writes. Recommend a bounded descriptor/quota/cache-budget proof, not production adoption; WASI integration, SQL, memory density, physical power loss and replica retention remain untested. No runtime/guest/dependency patch enabled; W-18's end-to-end gate remains open.
- **Historical ABI gate:** synchronous get/set trapped with `cannot block a synchronous task before returning`; filesystem controls and rollback passed. The obsolete experiment is removed. Q74 selects unchanged Spin async WIT, superseding the old interface-preservation requirement; no batch/atomics extension.
- **Goal:** reduce capability-specific host code while retaining one transactional filesystem for KV, SQL and direct files. Compare all replacement code, cold starts, memory, latency and storage amplification, not just host LOC. Do not introduce SQLite as a prerequisite for KV.
- **Config, if feasible:** move parsing/lookup from the `wasmtime_wasi_config` host integration into Wasm. Explore an existing capability for read-only deployment inputs (for example a shared read-only mount or environment-based injection), preserving app scope, immutability, absent-key behavior and exposure rules. Do not copy config into each object's writable volume or bake deployment secrets into published Wasm. The host still owns supplying inputs and enforcing access.
- **Composition gate:** synchronous WIT calls cannot block on async P3 filesystem I/O, as W-1 demonstrated. Establish a valid standard-interface/ABI strategy or preload mechanism first; fewer host handlers must not come at the cost of a custom backchannel, a quota bypass or more lifecycle machinery. Report separately whether KV and config genuinely reduce host coupling.
- **Acceptance:** run the existing KV/config integration contracts against composed providers, including KV quota/rollback cases and config isolation; inventory the host code removed and the bootstrap responsibilities retained. Compare against the current implementation and recommend keep/move independently for each.
- **Status:** in progress (migration and filesystem evaluation requested by owner)
