# Issues

One list of known problems (`N-n`) and planned work (`W-n`), so nothing is done twice or missed. Before starting one, set its status to `fixing` (or `in progress`), with who has it, and push; when it is on `main`, set `fixed in <commit>` (or `done in <commit>`) in the same push. New problems get the next `N-` number, new work the next `W-` number.

- **Reproduced**: shown against the real code or the OS. **From source**: traced through the code, not run.
- Status: `open`, `fixing`, `fixed in <commit>`, or `deferred: <why>`.

**Current delivery (2026-11-08):** W-9 local capture is implemented in `b7bf806`, passed CI 42491417145 and is included in this main integration. Production remote receipt, restoration, retention and fencing remain open; the next acknowledgement/RPO policy needs owner approval. Runtime is 5,320; 5k is a simplicity goal, not a hard limit. Both slower actor rewrites remain rejected. Backup/restore and N-43 remain delivered, not the next priority. Verify this main integration head's CI; no active runtime file ownership remains after landing.

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
- **Status:** partly done in 57d17a4: the runtime depends on `oci-spec` instead of oci-client (10 fewer crates in the runtime build; three small serde structs read the manifest fields it needs). The `check_digest` in `from_headers` stays on purpose: Reference parsing accepts upper case and other algorithms, and the early check answers a malformed digest with a 400 before any start lock or registry work. Its last part, dropping TLS from the proxy, is W-14 (decided). Since `cf/oci-push` oci-client is gone entirely (owner's decision): `nanovm publish`, `examples/dev-registry.rs` and the test helpers push through `registry::Oci::push_app`, and 7 more crates left the build.

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

- **Reproduced:** from source, not a live stalled-resolver test
- **Also (sec review):** the tag lookup that follows (`Oci::digest`, against the registry the record names) has the same gap: `read_timeout` is per read, so a registry trickling its manifest holds the lookup as long as a client waits. One deadline around `Cache::get`'s lookup covers both.
- **Status:** open; bound the initial lookup and add a stalled-resolver test

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

### N-70: a jailed worker can send datagrams to any unix socket on the host
- **Where:** `jail.rs` (the `socketpair` rule)
- **Scenario:** found by the sec review at `491e790`. The seccomp filter allowed `socketpair(AF_UNIX)` of any type. A datagram socket's `sendto` takes a destination even when connected, so code that got out of Wasm could send datagrams (and `SCM_RIGHTS` descriptors) to any named or abstract unix datagram socket it has DAC access to: journald and syslog (`/run/systemd/journal/socket`, `/dev/log`, both world-writable), systemd's notify socket, abstract sockets of other services. Landlock's filesystem rules do not cover socket sends (ABI 6's abstract-socket scope would block only abstract ones), so the README's "a worker's only channels are its two control sockets" did not hold.
- **Reproduced:** yes: `jail_probe` sent a datagram from the jail to an abstract socket the test bound (`probe unix_datagram=ok`); a Python PoC under the same Landlock ruleset also reached a pathname socket.
- **Status:** fixed in 32de17e: `socketpair` is allowed for `SOCK_STREAM` only (tokio's signal driver uses one); `jail_blocks_escapes` expects `unix_datagram=denied` and checks nothing arrived.

### N-71: a client can forge a WebSocket event by spelling its content type differently
- **Where:** `ws.rs` (`is_event`), `proxy.rs` (`forward`)
- **Scenario:** found by the sec review at `491e790`. The proxy refused a client request as a forged socket event only if its *first* `Content-Type` was exactly `application/websocket-events`. With `Content-Type: text/plain` followed by the events type, or the type in other case or with parameters, it reached the object, which may read any of them: the counter guest keeps the last header, and GRIP libraries strip parameters. The client also chooses `Connection-Id`, so it could speak as another user's socket, contrary to the README's "an object can trust `Connection-Id`".
- **Reproduced:** yes: through the proxy, counter answered a forged `TEXT id` with `m:7`, the id the client sent.
- **Status:** fixed in fdf0fee and bbc92f6: an event is any request with a `Content-Type` value (also each comma-joined part) whose media type is the events type, in any case, with any parameters. `the_proxy_routes_hosts_by_dns` sends two forgeries (400; 200 with `m:7` before), `upgrades_and_events_are_recognised` covers the spellings.

### N-72: a guest's publish costs the shared proxy 24 times its size in memory
- **Where:** `ws.rs` (`messages`), reached from `egress.rs` (`hold`, in the worker) and `Proxy::publish`
- **Scenario:** found by the sec review at `491e790`. Every publish (up to `max_response_body_kb`, 8 MiB) was parsed as a `serde_json::Value`, by the guest's worker when held and by the proxy, which every app shares. 8 MiB of tiny values beside `"items": []` peaked at 192 MiB and 200–370 ms of a proxy thread, from plain Wasm, once per request.
- **Reproduced:** yes (a counting allocator over the same parse): 8 MiB → 192 MiB peak.
- **Status:** fixed in fd7dfe4: publishes are read into typed structs and the rest is skipped unbuilt (the same body: under 1 MiB, 21 ms; 8 MiB of minimal items: 42 MiB). `publishes_parse_in_bounded_memory` fails with the old parser (1 MiB held 25 MB). What the proxy buffers across all apps is still unbounded: W-35.

### N-73: a compromised worker chooses the `Host` its allowed upstream sees
- **Where:** `egress.rs` (`send`)
- **Scenario:** found by the sec review at `491e790`. The supervisor matched a worker's request against the allowlist by its URI host, then sent the worker's own headers. Guests cannot set `Host` (wasi-http forbids it), but a compromised worker can: a request to an allowed name on a shared front end (a CDN), carrying the `Host` of a site the list never named, reaches that site (domain fronting). The public-address filter still held.
- **Reproduced:** yes: `the_upstream_hears_the_allowed_host` saw `Host: elsewhere.example`.
- **Status:** fixed in 9890e6d: `send` sets `Host` from the checked authority.

### N-74: a compromised worker's telemetry batch costs the supervisor 200 times its size
- **Where:** `telemetry.rs` (`Relay::handle` → `stamp`)
- **Scenario:** found by the sec review at `491e790` (sec's entry has the full text). The relay decoded each worker batch (up to 1 MiB) into prost structs to replace its resource and drop identity attributes. 1 MiB of 2-byte empty spans allocated 278 MB and took 308 ms of the supervisor's single control thread per batch.
- **Reproduced:** yes (`stamping_a_batch_costs_about_its_size` fails with the decoding stamp: 277,873,410 bytes allocated)
- **Fix:** `stamp` rewrites the batch at the wire level: each `ResourceSpans`/`ResourceLogs` resource is replaced by the stamped one (repeated resource fields included), and identity attributes (`service.name`, `service.version`, `nanovm.app`, or any key by string-table index) are still dropped from scopes, spans, events, links and log records, judging a key given twice by the last one, as decoders do. Everything else is copied as bytes. The span-level scrub stays: the Collector's span-metrics connector reads `nanovm.app` and `service.version` from span attributes before resource attributes. Hostile batch: 278 MB → 3.1 MB allocated, 308 → 9.9 ms. A normal 256-span batch: 837 → 176 µs, 802 → 120 KB allocated (release, loaded machine). `stamping_matches_decoding_and_scrubbing` checks the output against decoding and scrubbing; mutations (decode again, skip event attributes, first key wins, keep the worker's resource, ignore string-table keys) each fail it.
- **Status:** fixed in the perf branch (see `notes/perf.md`)

### N-75: a guest can hold many times its memory limit in host-side headers
- **Where:** `object.rs` (`new_store`: the resource table's capacity, `WasiHttpCtx`'s field size limit), `server.rs`/`link.rs`/`egress.rs` (hyper's head size)
- **Scenario:** found by the sec review at `491e790`. Each `wasi:http` `fields` resource held up to wasi-http's 128 KiB in worker memory, outside the object's `memory_mb`, and an instance may keep `max_resources` (10,000) resources across requests: about 1.25 GiB against a 64 MiB cap. Worse, the limit applies only to maps the guest builds: `clone` of a received map (a request's headers, up to hyper's ~400 KB head) shares it, and the first change copies it whole before any size check, so 10,000 copies are about 4 GiB. Its own app's worker suffers first (other objects, or its cgroup's OOM kill); without a worker cgroup, the node.
- **Reproduced:** yes: with `memory_mb = 32`, the test probe held 600 maps of 60 KiB, new or copied from a 60 KiB request header (36 MB each way), and a 100 KiB map.
- **Status:** fixed in 98c1698: a head is at most 64 KiB at every hop (hyper's `max_header_size`, which also bounds trailers; over it a client gets 431), a guest's map at most 64 KiB, and an instance holds at most `memory_mb / 64 KiB` resources (`max_resources` is the lower of the two: 1,024 at 64 MiB, 256 at the counter's 16 MiB). So header maps the host holds for an instance stay within its memory limit, and 64 KiB is twice what nginx accepts by default (four 8 KiB header buffers). A request's method, authority and path were left out: N-691. `guest_header_maps_are_bounded_by_the_memory_limit` (a test-only probe route) fails without each part.

### N-502: a failed create under a file bricks the object if its request then fails
- **Where:** `journal.rs` (`recover`, the `Undo::Created` arm; `creating` records the create first)
- **Scenario:** found by the quality agent's model-based quota test (D-46). A guest creates `d/a` (a file or a directory) where `d` is a file: `creating` records `Created(d/a)` (its `symlink_metadata` fails with ENOTDIR, not NotFound), and the create then fails. If that request fails (an error answer, a trap, CPU, a crash), the next start recovers the log; `symlink_metadata(d/a)` fails with ENOTDIR again, which recovery treated as an error, so the journal refuses to start the object, every time: the object answers 503 `instantiate-failed` (`Not a directory`) for good. Any app that creates paths from request data can be bricked by a client who makes it create under a file and then fail.
- **Reproduced:** yes: `quota_usage_always_matches_the_model` shrank it to two requests (commit an empty file `d`; then `mkdir d/a` in a request that fails); `a_failed_create_under_a_file_rolls_back` fails without the fix (`recovery succeeds: NotADirectory`).
- **Fix:** recovery treats NotADirectory like NotFound for a `Created` record: nothing can be there, so the create never happened. One line.
- **Status:** fixed in 6fdb057 (recovery); the class is closed on `ideas/recover-class`: `creating` and `removing_dir` refuse what they can't name instead of recording it (see N-503, N-504)

### N-503: a failed create of an over-long name bricks the object if its request then fails
- **Where:** `journal.rs` (`creating`, `recover`)
- **Scenario:** found by the quality agent's second hostile guest (D-58). The N-502 class with another errno: a 256-byte name fails `symlink_metadata` with ENAMETOOLONG, is recorded as `Created`, the create fails, and a failed request leaves recovery failing on every start (503 `instantiate-failed`, `File name too long`). Any app that names files from request data can be bricked by a long name and a failing request.
- **Reproduced:** yes (`unusual_names_commit_and_roll_back` on `ideas/hostile2`; `random_path_shapes_always_roll_back` fails without the fix). `records_of_operations_that_cannot_succeed_recover` checks what the journal does record of operations that can't succeed: a create under a missing directory, and a rename to under a file or under a name past NAME_MAX whose cancellation a crash kept from being durable.
- **Fix:** see N-504.
- **Status:** fixed on `ideas/recover-class`

### N-504: a directory chain deeper than PATH_MAX bricks the object
- **Where:** `journal.rs` (`creating`, `removing_dir`, `renaming`, `recover`)
- **Scenario:** found by the second hostile guest (D-58). A guest reaches any depth by opening each directory from its parent's descriptor; the journal names entries by absolute path. Creating a directory whose path reaches PATH_MAX (16 levels of 255-byte names under a 72-byte volume path) was recorded, made, then failed the commit (`File name too long`) and every later recovery: the object answered 503 `instantiate-failed` for good. A directory renamed under a deep one could also move what is under it out of the journal's reach.
- **Reproduced:** yes (`/deep` on `ideas/hostile2`: depths 12-15 commit and roll back; 16-20 brick the object).
- **Fix (one class with N-502, N-503):** `creating` and `removing_dir` record only when `symlink_metadata` says NotFound (or, for `removing_dir`, a directory); any other error is returned, so the guest gets the error the syscall would give (`not-directory`, `name-too-long`) and nothing is recorded. Returning rather than recording nothing: for a whole path too long for the journal but not for the guest's descriptor, the create would succeed out of the journal's reach. `renaming` refuses a move that would put anything under it at PATH_MAX or more (walking a moved directory, at most `max_files` entries). Recovery treats path-shape errors (NotFound, NotADirectory, ENAMETOOLONG, and ELOOP) as "nothing there" for every record; real I/O errors stay fatal (since N-690, ENAMETOOLONG only for a name past NAME_MAX: recovery reaches entries through descriptors).
- **Status:** fixed on `ideas/recover-class`


### N-551: a supervisor running as pid 1 (a container without an init) starts no worker
- **Where:** `jail.rs` (`Jail::enter`: `ensure!(getppid() != 1, "supervisor exited")`)
- **Scenario:** found by the ops agent building the container image. A worker took "my parent is pid 1" to mean its supervisor had died before `PR_SET_PDEATHSIG` was armed. In a container without `--init`, or a Kubernetes pod, the supervisor *is* pid 1, so every worker refused to start (`502 worker-down`, worker's last words `entering the jail: supervisor exited`). The check also passed wrongly under a child subreaper, which adopts orphans instead of pid 1.
- **Reproduced:** yes, `deploy/container/test/local.sh` ("as PID 1, workers start" fails before the fix).
- **Fix:** the supervisor puts its pid in the worker's spec, and the worker compares `getppid()` with it. +2 runtime lines.
- **Status:** fixed on `ideas/container-image`

### N-552: a binary replaced under a running supervisor starts workers of the new version
- **Where:** `main.rs`/`supervisor.rs` (`Supervisor::new(cfg, std::env::current_exe()?, …)`; `start` spawns `self.exe`)
- **Scenario:** found by the ops agent writing the upgrade runbook. The supervisor resolved its executable's path once at start and spawned every worker from that path. Installing a new binary (`install`, `mv`, a package manager's upgrade before its restart) while the node ran made new workers the new version under the old supervisor; their private spec or link protocol can differ.
- **Reproduced:** yes, `docs/ops/drills/binary-swap.sh` with builds before and after N-551: the running app kept serving, a new app got `502 worker-down` (`worker spec: invalid length 3, expected a tuple of size 4`).
- **Fix:** the supervisor spawns `/proc/self/exe`, which in the forked child is the supervisor's own image even after its file is replaced or deleted (argv[0] stays `nanovm`, so `ps` is unchanged). +2 runtime lines.
- **Status:** fixed on `ideas/worker-exe` (the same drill: the new app starts from the running image)

### N-200: an in-process node could not publish to its sockets
- **Where:** `egress.rs` (`send_request`), `object.rs` (`commit`)
- **Scenario:** a node built in-process (tests, the benchmark, embedders, `nanovm run`) sent a guest's `http://nanovm/publish/` to its egress policy, which denied it, so only jailed workers could publish, and nothing could run a WebSocket app on one machine without the supervisor and proxy.
- **Status:** fixed on `cf/onramp` (416f85d, ported from 1392e99): every node holds publishes until its request commits; a jailed worker forwards them as before, an in-process node hands them to `Node::sockets` (the proxy's `ws::Sockets`). `only_well_formed_publishes_are_held` fails without it.

### N-201: a publish to another `http://nanovm/` path was accepted and dropped
- **Where:** `egress.rs` (`hold`)
- **Scenario:** a worker held a POST to, say, `http://nanovm/elsewhere` as a publish and answered 200; the supervisor then dropped it, so the guest believed it had published.
- **Status:** fixed on `cf/onramp` (416f85d): anything but `/publish` gets 404 at once.

### N-202: the QuickJS runtime's allocator declarations trip rustc 1.99's runtime-symbol lint
- **Where:** `js/runtime/src/lib.rs` (`extern "C"` malloc, calloc, realloc, free)
- **Scenario:** declared with `*mut u8` where std's runtime symbols take `c_void`; rustc 1.99 warns (`suspicious_runtime_symbol_definitions`). Harmless on wasm32 today (same ABI) but a future toolchain may make it an error.
- **Status:** fixed on `cf/onramp` (3308ee0): `c_void` signatures, cast at the allocator boundary.

### N-203 (tests): `kv_counts_against_the_disk_quota` fails under load on the base too
- **Where:** `tests/integration.rs`
- **Scenario:** the test writes 198 one-byte keys in one `hostile` request, under that app's 100 ms CPU budget and 1 s deadline. At load average 13–19 from other agents it failed with `cpu` or `timeout` in 4 of 4 alternating runs of `cf/main-next`'s test binary and 4 of 4 of `cf/onramp`'s (one run of each in the first round produced no result), and passed alone at lower load. It measures the machine, not the quota.
- **Status:** open: give the fill its own app limits (as `kv_refuses_bad_names_and_streams_keys` does for its 1,500 keys), keeping the quota assertions.

### N-600: SQLite's own syncs are charged to the request's CPU budget
- **Where:** `components/sqlite/provider` (`PRAGMA journal_mode=DELETE; synchronous=FULL`, the VFS's `block_on` around `sync`), CPU accounting in `object.rs` (`charged`: time inside the store's polls)
- **Scenario:** found by the examples (`examples/leaderboard`, `examples/shop`). Every SQLite statement that writes is a SQLite transaction with its own journal file and syncs, and the provider waits for each host fsync inside the guest's poll, so the wait counts as CPU. Under `nanovm run` (debug build, 4 cores at load 21-26), one-row `INSERT` statements cost ~6-12 ms each: 10 in one request took 97 ms, 20 took 243 ms, 40 were refused with `cpu` ("used more than 200ms of CPU in one request"; 200 ms is the most `nanovm run` allows). A checkout of four writing statements and one call failed with `cpu` in 1 of 4 runs. The same rows as one statement (`INSERT … SELECT … FROM json_each(?)`): 100 rows in 25-47 ms. The examples write one statement per request (a batch statement; a trigger for multi-table changes) and say why.
- **Why (from source):** `object.rs` builds the WASI context with `allow_blocking_current_thread(true)`, so wasmtime-wasi runs a guest's `sync`/`sync_data` (and every other file call) inline, inside the store's poll that `ObjShared::charged` times. The journal's own waits are given back with `uncharge` (`journal.rs`), but `QuotaFs`'s `sync`/`sync_data` delegate straight through (`quota.rs`), so a guest's own fsyncs count as CPU. Only SQLite issues them today (synchronous=FULL, several per writing statement).
- **Reproduced:** yes (numbers above; `leaderboard_pages_and_ranks_from_sqlite` and `shop_checkout_is_all_or_nothing` were the failing tests before the examples were changed).
- **Status:** open, for the storage owners: nanovm's outer journal already makes the request atomic and durable, so SQLite's DELETE journal and FULL syncs are redundant work (experiments/sqlite-filesystem found MEMORY mode recovers correctly, "not a general endorsement"). Either the provider syncs less (e.g. `synchronous=OFF` or `journal_mode=MEMORY`, after the recovery matrix is rerun), or `QuotaFs`'s `sync`/`sync_data` uncharge their wait as the journal does (a few lines in `quota.rs`).

### N-601 (tests): restarting `nanovm run` raced the old process's storage lock
- **Where:** `tests/dev.rs` (`drop(run); start(..)`), and the examples harness as first written
- **Scenario:** `kill_on_drop` sends SIGKILL without waiting; a `nanovm run` started on the same data before the old one has exited fails with "app storage is already owned or cannot be locked" and exits. Seen in 2 of ~16 runs of the shop example's test at load ~25.
- **Status:** fixed on `ideas/examples` (42b2fc2 for the examples harness, fa88fe7 for `dev.rs`): kill, then wait for the process, then start.

### N-602: `nanovm run` lowered a manifest's limits without saying so
- **Where:** `main.rs` (`run`: `spec.limits.clamped(&cfg.limits)`)
- **Scenario:** `nanovm run` caps every limit at its node's defaults (1 MiB request bodies, 200 ms CPU, 5 s deadlines). A manifest asking for `max_request_body_kb: 4096` got 1024, and a 3 MiB upload was refused with `413` (found by `examples/files`); nothing at startup said the manifest had been overridden.
- **Status:** fixed on `ideas/examples`: `run` prints a line for each limit it lowers (`the manifest's max_request_body_kb is 4096; nanovm run allows at most 1024, so it is 1024 here`). `nanovm_run_says_when_it_lowers_a_limit` times out waiting for that line without it. Raising the caps (a flag, or the manifest's own limits up to a higher ceiling) is left open.

### N-603: the SQLite provider refused full-text search
- **Where:** `components/sqlite/provider/src/lib.rs` (the authorizer)
- **Scenario:** the provider denies every `PRAGMA` but `integrity_check`, and the authorizer also sees the statements SQLite's own modules prepare: FTS5 asks `PRAGMA main.data_version` (FTS3/4 and R-Tree ask `page_size`), so `CREATE VIRTUAL TABLE … USING fts5` and every write to one failed with `access-denied`, although the bundled SQLite has FTS5 compiled in (found by `examples/notes`).
- **Status:** fixed on `ideas/examples`: the two read-only pragmas (no value) are allowed; setting pragmas stays denied (the SQLite experiment's policy test, which checks `journal_mode=WAL` and `synchronous=OFF`, is unchanged). `notes_search_their_own_full_text_index` fails with `Error::AccessDenied` without it.

### N-180: a cycle of calls closed through waits on two other nodes is ended by the timeout, not refused
- **Where:** `table.rs` (`wait_for`, `dispatch`), with leases
- **Scenario:** a call carries its callers, so a node refuses (508) any cycle along one chain of calls, and any whose waits are recorded on that node. Concurrent requests whose waits are recorded on two or more other nodes (x on A waits on y on B, y waits on z on C, z calls x) can still close a cycle: it holds until the request timeout. Found while building W-10.
- **Status:** open; refusing it needs a wait graph shared between nodes (owner's call: notes/multinode.md)

### N-181: with leases, the lease store is a hard dependency of every node
- **Where:** `lease.rs`
- **Scenario:** a node that can't renew its session for 7/8 of `lease_ms` stops serving all its objects; if the store is unreachable from every node, the whole cluster stops after `lease_ms`, and new objects can't be placed meanwhile (the proxy retries for `failover_ms`, then 503). Inherent to lease fencing (etcd and Consul behave alike); S3's availability bounds the cluster's.
- **Status:** open, owner's call (notes/multinode.md)

### N-182: with more than one node, the proxy buffers each request body to retry it
- **Where:** `proxy.rs` (`send`)
- **Scenario:** to send a request again after a 421 or to the next node, the proxy holds its body, up to `max_request_body_kb` (1 MiB), per request in flight; one node still streams. Adds to what W-35 bounds.
- **Status:** open (with W-35)

### N-183: each instantiation with leases costs a store round trip
- **Where:** `lease.rs`, `object.rs` (`run`)
- **Scenario:** an object's actor takes its lease (a read and a conditional write) before instantiating and releases it (a write) when it exits, so a cold start waits on the store (tens of ms on S3) and LRU churn costs three store requests per instantiation.
- **Status:** open; W-91 would keep released leases for a while. Since `fix/lease-r3fix` each acquisition also claims the object's head (a read and a conditional write), and a takeover restores the object from the store before its first request.

### N-640: the CPU budget charged time waiting for a core, so overload became CPU kills
- **Where:** `object.rs` (`ObjShared::charged`, `cpu_used_ns`, `uncharge`, the epoch callback), `journal.rs` (its three `uncharge` sites)
- **Scenario:** found by the orchestrator on a 4-core VM at load ~20. The per-request budget read the wall clock before and after each poll, so time the guest's thread spent descheduled, or blocked in an fsync the guest asked for, was billed as CPU. On an overloaded node honest requests got 500 `cpu`, and the restarts added load. `sqlite_repeated_updates_remain_bounded`, `sqlite_handled_storage_exhaustion` and `sqlite_journal_pressure_matrix` (5–10 s budgets) and `kv_counts_against_the_disk_quota` (100 ms, see N-32) failed under load and passed idle. N-600 (examples agent) is the same bug: the SQLite provider's synced one-row writes were charged their fsync waits.
- **Reproduced:** yes. `cpu_budget_is_cpu_not_time_waiting_for_a_core` runs the node on one CPU shared with seven busy threads: before, a request needing about 33 ms of CPU was killed after 12–16 ms of CPU, and `/spin` after 14–18 ms, against a 100 ms budget (5 runs). `sqlite_sync_waits_are_not_charged_as_cpu` (100 synced one-row transactions, default 200 ms budget) failed 3 of 3 at load 16.
- **Fix:** each poll is charged the change in the polling thread's `CLOCK_THREAD_CPUTIME_ID`: user and kernel time, so the guest's own file calls count and waiting for a core or the disk doesn't. The epoch callback runs on that thread and reads the poll in progress the same way; nothing reads it from another thread. The journal's own work (undo records, renames into the journal, the commit and any capture) stays uncharged, measured with the same clock instead of as wall time, so no request is charged more than before. `guest_cpu_ns` is now CPU time. The wall-clock deadlines (504) still stop requests that are slow for any reason. After: the test's request succeeds with 31–40 ms of CPU and `/spin` is killed after 102–103 ms (5 runs); the SQLite tests above and the new one pass under load 21–37 where the old build fails 4 of 5. Tests whose fixtures assumed wall billing get deadlines sized for a busy disk (`filler`, the two 1,000-statement SQLite tests, the 1,500-key KV test) or for lane turns (`cpu_hogs_leave_cores_for_short_requests`), and the quota test `kv_counts_against_the_disk_quota` moves to `filler` like its siblings: its 198 writes use 100–160 ms of real CPU on this loaded host, half of it kernel time. Cost: about 0.16 µs per clock read, 8.1 reads per warm GET (≈2%). Details: `notes/cpuclock.md`.
- **Status:** fixed on `cf/cpu-clock`

### N-160 (efficiency): a small durable write cost 9 device flushes
- **Where:** `journal.rs`
- **Scenario:** the first record of every request made the log and synced three directories; every record was synced on its own; commit synced each changed path, deleted the log and synced its directory. A counter `POST /incr` (create, write, rename over) took 6 fsync + 3 fdatasync, 23 device writes, 2 discards and 9 flushes (private nojournal ext4 loop).
- **Fix:** the log is made once per object and kept; its first block holds a format tag and a random key that signs every record, and a commit writes a new key (one overwritten block, synced) instead of deleting the log. Records made back to back share a sync. Now 15 writes, 1 discard, 5 flushes; journaled ext4 16.9 writes + 12.7 flushes -> 9.5 + 5.5.
- **Status:** fixed in d464eec, 1aec24f (cf/storage)

### N-161 (efficiency): a commit blocked its worker thread, and every object on it
- **Where:** `quota.rs` (`QuotaGuard::commit`), `object.rs` (`commit`)
- **Scenario:** a commit's syncs ran inline on a tokio worker: a 4-thread worker had at most 4 commits in flight though fdatasync here does 543/s from one thread and 11k/s from four.
- **Fix:** the commit runs on a blocking thread and is awaited; its syncs of changed paths go up to 16 at once. `block_in_place`, tried first, hung `post_return_work_keeps_context_and_reports_commit_failure`: do not use it inside wasmtime's event loop.
- **Status:** fixed in 1aec24f (cf/storage)

### N-162: a request that wrote many files overran its post-return deadline committing them
- **Where:** `journal.rs` (`commit`)
- **Scenario:** `kv_refuses_bad_names_and_streams_keys` writes 1,500 keys; their commit synced 1,500 files one by one, longer than the 5 s post-return deadline on a loaded disk: the instance was killed and the request reported a timeout though nothing was wrong.
- **Fix:** paths sync up to 16 at a time (N-161). Still open: the same test (and `kv_counts_against_the_disk_quota`) can exceed the 10 s request deadline under load average ~20, at 2 record syncs per key (temp-file create, rename), as on base.
- **Status:** commit part fixed in 1aec24f; record syncs per KV key open

### N-163: a guest's sync reached the disk
- **Where:** `quota.rs` (`sync`, `sync_data`)
- **Scenario:** the stock host fsyncs on a guest's `sync`/`sync-data`, though a request's writes are durable at its commit and rolled back otherwise, synced or not. The SQLite provider syncs ~1.7 times per write statement (strace: 120 fsyncs over 70 `POST /set`), and a guest could make the device flush in a loop without writing anything.
- **Fix:** both are a validated no-op.
- **Status:** fixed in 1aec24f (cf/storage)

### N-164 (efficiency): capture cost 8 syncs and a full-volume copy per commit
- **Where:** `committed.rs`, `journal.rs`
- **Scenario:** W-9 capture (b7bf806) synced the archive directory and its two ancestors on every commit, staged in the journal directory and published by a hard link with two more directory syncs, and copied the whole volume each time: `POST /incr` with capture took 17 flushes and 39.8 writes, against 9 and 22 without.
- **Fix:** versions are changes against the one before (snapshot every 16), staged beside the published ones under a name tied to the journal key, published by an unsynced same-directory rename that recovery redoes: 7 flushes and 20.9 writes with capture, 5 and 15.0 without.
- **Status:** fixed in bc8f45a (cf/storage)

### N-165 (efficiency): the shipper polls every object's capture directory
- **Where:** `ship.rs` (`run`)
- **Scenario:** every 20 ms the shipper lists `~committed/` of every app and stats each object's directory, skipping unchanged ones: ~0.1 core at 10k objects, and up to 20 ms added to a `remote_ack` commit. A worker can't notify the supervisor's shipper except through the filesystem.
- **Fix (proposed):** a notification over the worker's supervisor link when a version is published.
- **Status:** open

### N-166 (efficiency): restoring an object downloads every version it ever had
- **Where:** `ship.rs` (`restore`)
- **Scenario:** keys are probed from sequence 1 until one is missing; only versions from the last snapshot on are needed, and the store keeps everything (no remote retention yet).
- **Fix (proposed):** a pointer object to the last shipped snapshot, written after it, and retention of versions before the second-last snapshot.
- **Status:** in part on `fix/lease-r3fix`: the object's head names its last version, and a restore follows parent digests back from it to the last snapshot, fetching only those. Retention (deleting versions before the second-last snapshot) remains.

### B-500: recovery was offline and whole-node only (beta audit)
- **Where:** `scripts/backup.py` (restic), `ship.rs`, `supervisor.rs`
- **Scenario:** backup needed the node stopped; restore replaced the whole root, so every tenant lost what it wrote since the snapshot; the store that shipping fills had no restore command; no restore of one app or object.
- **Fix:** shipping every committed version to `capture.store` is the backup; restic and `scripts/backup.py` are gone (AGENTS.md rule 1: no old way kept for its own sake). `nanovm admin … restore <app> [<object> [<version>]]` restores into a running node through the operator socket: the object is paused (`ship::pause`; `503 object-restoring`; its instance stopped through the worker), its unshipped versions shipped, the version fetched and checked, staged under `<data_dir>/restore/`, decided (`ready`) and installed with the journal; a version older than the head becomes the version after it. `ship::recover` finishes or drops a restore a crash cut short, at supervisor start. `versions` and `backlog` commands. Evidence: `notes/beta-recovery.md`.
- **Status:** fixed on `beta/recovery`

### B-501: a store outage became a write outage after 16 versions, and its backlog copied the volume every 16
- **Where:** `committed.rs` (`prepare`), `config.rs` (`CaptureConfig`)
- **Scenario:** with `remote_ack` off (one node), an object's writes were refused once 16 versions waited (`max_versions`): a 16-write outage of the store. And every 16th version was a snapshot of the whole volume, so a long outage's backlog grew with the volume's size, not with what was written.
- **Fix:** `max_versions` is gone; `max_mb` (bytes, per object) bounds what waits. A scheduled snapshot is skipped while more than `snapshot_every` versions wait. The backlog is visible (`admin backlog`, `nanovm.capture.waiting.*`, a log line when it starts, every minute, when it ends). `remote_ack` (several nodes) is unchanged. Test: `writes_go_on_through_a_store_outage_and_ship_in_order_after` (60 writes with the store down, then everything ships in order; snapshots only at 1 and 17); mutation-checked (a 16-version bound back; snapshots every 16 regardless).
- **Status:** fixed on `beta/recovery`

### B-502: the supervisor, root, wrote through paths in an app's directory, which the app's worker controls
- **Where:** `ship.rs` (`restore` staged under `objects/<app>/~restore/`; `mark`; `discard`; supervisor's `restored` chown)
- **Scenario (from source):** with leases, a claim restored an object as root by writing the store's versions to `objects/<app>/~restore/<object>/<n>.tar` with `create_dir_all` and `fs::write`, which follow symlinks. A compromised worker (uid of its app) owns `objects/<app>` and can make `~restore` a symlink: root then creates directories and writes the app's own bytes under a path of the worker's choosing outside the jail. `mark` (`~shipped/~new`, written with `fs::write`) and `discard` (`remove_dir_all` through an intermediate symlink) have the same shape for every node with a store.
- **Fix:** a restore stages under `<data_dir>/restore/<app>/<object>` (no jail reaches it), is given to the app's uid there, and is installed by descriptor (`openat` with `O_NOFOLLOW` at each name, `renameat`, `remove_at`), as are the directories it makes. `mark` and `discard` are left to the `r3-lease` fix (`fix/r3-lease` rewrites both): they should open `~shipped` and the object's directories the same way.
- **Status:** fixed for restore on `beta/recovery`; `mark`/`discard` open (with `fix/r3-lease`)

### B-503: a commit could fail with "gap in committed versions" while the shipper pruned
- **Where:** `committed.rs` (`head`)
- **Scenario:** reproduced by `restores_an_object_and_an_app_into_a_running_node`: an app writing steadily while its versions shipped got `500 commit-failed: gap in committed versions` at its 25th write. The shipper removes shipped versions (all but the last) while the worker lists the directory; a listing can see some removed ones and not others (in any order), which `head` took for a gap.
- **Fix:** `head` takes the last version it sees and doesn't require the others to be contiguous (nothing else can make a gap: versions are published in order, and only the shipper removes them). Test: `a_commit_goes_on_past_versions_pruned_out_of_order`; mutation-checked.
- **Status:** fixed on `beta/recovery`

### B-504: the shipper shipped a version naming another app or object as this one's
- **Where:** `ship.rs` (`ship_object`)
- **Scenario (from source):** the shipper (root) read `~committed/<object>/<n>.tar`, which the worker writes, and checked only its sequence, parent and epoch. A compromised worker could put there (or symlink to) a version of another object, or another app's, and have it shipped and restored as its own.
- **Fix:** a version ships only if its identity names this app and object; a restore checks the same of each version it fetches. Test: `a_version_naming_another_object_is_not_shipped`; mutation-checked.
- **Status:** fixed on `beta/recovery`

### B-505: a fresh node pointed at an existing store serves its objects empty until they are restored
- **Where:** `ship.rs`, `object.rs`
- **Scenario:** with one node there is no claim before serving, so a node started on an empty root with the store of a lost node serves each object empty; a write then makes version 1, which can't ship (the head is further on), so it waits until `max_mb` and the object's writes are refused. The runbook says to restore with ingress off.
- **Fix (proposed):** an object with no history here asks the store for its head before its first instance (as a claim does with leases), restoring it if the store has one. It costs a store round trip per cold start of an object never written here (N-183's cost); an owner decision.
- **Status:** open

### B-506 (efficiency): restoring an older version fetches every version after it
- **Where:** `ship.rs` (`restore`)
- **Scenario:** the version to restore is found by following parent digests back from the head, so every version between it and the head is fetched whole; only their identities (the first 4.5 KiB of each) are needed.
- **Fix (proposed):** a ranged read of each version's identity on the way back.
- **Status:** open

### N-500: only the import allowlist keeps a guest's WASIp2 file writes out of the quota and journal
- **Where:** `app.rs` (`linker`: `wasmtime_wasi::p2::add_to_linker_async`; `check_imports`), `object.rs` (`/data` and `/tmp` preopened read-write in the `WasiCtx`)
- **Scenario:** found by the quality agent while writing D-52. The node's linker defines every WASIp2 interface, including `wasi:filesystem@0.2` and `wasi:sockets@0.2`, and the WASIp2 filesystem shares the `WasiCtx`'s read-write preopens. A guest using `std::fs` links (`instantiate_pre` succeeds). Today `ALLOWED_IMPORTS` refuses it at load, by name, so nothing is exploitable; but that check is the single layer between a guest and writes to `/data` that bypass the quota and the journal (no rollback). Sockets have a second layer (wasmtime's default address check denies every address).
- **Reproduced:** yes (`components_importing_refused_interfaces_are_rejected_by_name`: `p2_filesystem` and `p2_sockets` link against `app::linker`; with `check_imports` disabled, `p2_sockets was loaded`).
- **Suggestion:** link only the WASIp2 interfaces `ALLOWED_IMPORTS` permits (io, clocks, random, cli), so a regression in the allowlist fails at link time instead of opening `/data`. Defense in depth, not a bug.
- **Fix:** `app::linker` defines only the WASIp2 interfaces the allowlist permits (io, clocks, random, cli), so the fixtures fail to link (`wasi:sockets/…`, `wasi:filesystem/…` not found) even without the check. Building the linker got cheaper (median 148 → 90 µs); instantiation is unchanged (`notes/hardening.md`).
- **Status:** fixed on `cf/hardening`

### N-680: a link connection answered before its request body was sent went back to the pool, holding the body
- **Where:** `link.rs` (`LinkBody::poll_frame` pooled the connection once the response ended if it was still open)
- **Scenario:** found by the hardening agent while testing N-505 through the supervisor. A worker that answers before reading the whole request body (a 408 `body-timeout`, or a 413) leaves the supervisor's link connection still sending that body. If the response's last frame was read before the connection saw the worker hang up, the connection went into the idle pool, where nothing polls it: it kept the client's body, and so the client's connection to the node, open indefinitely, with the 408 already sent. `checkout` never reuses such a connection (it isn't ready), so it only leaked.
- **Reproduced:** yes: 1 run in 5 of `slow_request_bodies_are_cut_at_every_entry_point`, the supervisor's client got its 408 and the connection stayed open (40 s and more); `a_connection_answered_before_its_body_is_sent_is_not_pooled` fails every time without the fix.
- **Fix:** a connection is pooled only if it is also ready for the next request (`SendRequest::is_ready`, the check `checkout` already made); otherwise it is dropped, which drops the body and lets the client's connection close. No new runtime lines.
- **Status:** fixed on `cf/hardening`

### N-681: the node's front end has no header timeout
- **Where:** `main.rs` (`front` → `server::serve_http1` → `serve_conn(…, None, …)`); hyper's default 30 s header timeout needs a timer, which isn't set
- **Scenario:** found by the hardening agent while fixing N-505. The proxy gives a client 10 s for a request's headers; the node's own listener gives none, so a client that reaches the node directly can hold a connection (and an fd) by sending a request head a byte at a time. Bodies are bounded since N-505; heads are not. Behind the proxy, as deployed (the node listens on loopback), only the proxy reaches it.
- **Reproduced:** by reading: `Dur::Default` timeouts are dropped without a timer (`hyper::common::time::Time::check`). Not run.
- **Suggestion:** set a header timeout at the node longer than the proxy's pool idle timeout (hyper-util's 90 s default), or the node would close idle pooled connections as the proxy reuses them; or lower the proxy's pool idle timeout below the node's.
- **Status:** open

### N-690: recovery dropped the undo of a real entry whose whole path is past PATH_MAX
- **Where:** `journal.rs` (`recover`)
- **Scenario:** found by the independent review of PR #1 (a regression from N-504). Recovery took ENAMETOOLONG as "nothing there" for every record. But the log's paths are relative to the data root, which can move deeper between a crash and the recovery after it (a backup restored into a longer root): a real entry whose whole path fitted is then past PATH_MAX while each of its names fits. Recovery skipped the record, truncated the log and kept the uncommitted entry, so the failed request's changes were partly kept.
- **Reproduced:** yes: `long_paths_after_the_root_moves_are_undone` (a change of each kind, recorded through the journal, to 200-byte names whose whole paths fit; then the root moves about 780 bytes deeper): recovery by path keeps the changes; through descriptors it undoes them all.
- **Fix:** recovery reaches each entry through its parent's descriptor, opened one name at a time from the base (`openat` with `O_DIRECTORY | O_NOFOLLOW`), and undoes and syncs it relative to that parent (`fstatat`, `unlinkat`, `mkdirat`, `renameat`, `openat`). ENAMETOOLONG then only means a name past NAME_MAX, which nothing can have. N-502..N-504's tests pass unchanged, and new writes still refuse paths at PATH_MAX.
- **Status:** fixed on `cf/main-next`

### N-691: a guest's request methods, authorities and paths were held on the host without bound
- **Where:** `app.rs` (`linker`: wasi-http's `request.set-method`, `set-authority`, `set-path-with-query`, `set-scheme`)
- **Scenario:** found by the independent review of PR #1 (it predates the PR). N-75 bounds what a resource holds by its header map, but a request also holds the method, scheme, authority and path the guest sets, on the host. An extension method (`http::Method`'s extension storage) and an authority took any length, and a path up to `http`'s 64 KiB limit beside 64 KiB of headers. A guest that never sends these requests keeps them all for the instance's life.
- **Reproduced:** yes, with a real guest at `memory_mb = 16` (`guest_request_heads_are_bounded_by_the_memory_limit`, the test-only probe's `/strings` route): 32 methods of 1 MiB, 32 authorities of 1 MiB and 200 paths of 60 KiB beside 60 KiB of headers were all taken and held (33,554,432, 33,554,432 and 24,576,000 bytes), and each request answered 200.
- **Fix:** those four setters are shadowed in the node's linker by the same setters behind a check: one that would take the request's head (headers, counted as wasi-http's field limit counts them, plus method, scheme, authority and path) past `MAX_HEAD` (64 KiB) is refused with the setter's WIT error. That is the head the node accepts from a client, so a request resource still holds at most 64 KiB and N-75's resource cap keeps the instance within its memory limit. Responses hold only a status code and request options only durations, so nothing else is guest-sized. After: all 264 sets refused, nothing held; an extension method, an 8 KiB path with 8 KiB of headers, a 60 KiB path alone and a 253-byte host are still taken.
- **Status:** fixed on `cf/main-next`

### N-692: held publishes kept the guest's whole request, headers and all
- **Where:** `egress.rs` (`hold`, `EgressHooks::held`), `object.rs` (`commit`)
- **Scenario:** found by the independent review of PR #1 (it predates the PR). A publish to `http://nanovm/publish/` is held in the worker until its request commits. `hold` bounded what a request holds by the publishes' bodies (`max_response_body_kb`), but kept each whole request: the guest's headers (up to N-75's 64 KiB a request), its URI and the request itself. Sent requests have left the resource table, so N-75's cap no longer counts them, and nothing else bounded them: 300 publishes of a 12-byte body with 60 KiB of headers held about 18 MB, past a 16 MiB object's memory; 5,461 empty-item publishes held 25 times their bodies. The supervisor reads none of it: it forwards a publish's body with the app it names and the object the worker names.
- **Reproduced:** yes: `held_publishes_are_bounded_like_one_publish` (unit): 300 such publishes retained 18,524,700 bytes against an 8 MiB publish limit; 5,461 tiny ones 1,660,144 bytes against 64 KiB.
- **Fix:** only the body is held, and the commit sends it as `POST http://nanovm/publish` named with the object's id (what `hold` used to add), to the supervisor or, for an in-process node, to its own sockets (N-200). `hold` checks the method as well as the path (N-201), answering 404 as the supervisor would. Each held body counts its slot toward the limit too, under one lock. After: 13,200 bytes and 65,516 bytes (1,489 publishes).
- **Status:** fixed on `cf/main-next`

### N-693: compatibility with data an earlier build wrote (round-3 review S2): not supported
- **Where:** `committed.rs` (`adopt`, a reader of main's format-1 versions), `journal.rs` (`recover`), `lease.rs` (a session record without `lease_ms`)
- **Scenario:** round-3 review S2 asked that a node keep what main's build left in its data directory: a capture main had decided but not published, and main's whole-volume versions without an epoch. 7d03ade built that: recovery adopted main's staged capture, and versions were read in main's format as well as this one. Separately, M4 (3acd297) judged a session record without `lease_ms` by the taker's own.
- **Status:** not supported, see AGENTS.md rule 1: there is exactly one version of nanovm, v1. All three were removed on `fix/no-compat`. A version says `"format": 1` and is read only so; a log is refused unless its first block is the `nanovmj1` tag and a key; a session record without `lease_ms` doesn't parse and is refused. Operators start from a fresh data directory.

### N-710: work after the answer could fail the answer (round-3 review 2, D4 findings 1 and 4)
- **Where:** `js/runtime/src/lib.rs` (`handle`), `sdk/rust/src/lib.rs` (`__serve`, `after`), `object.rs` (the future `response.new` gives the guest)
- **Scenario:** D4 made a failed `ctx.waitUntil` abort the instance so that its writes after the answer roll back. But the work ran from the moment the handler returned, so the abort could land before the node had taken the answer: a promise that can never settle (found on the first poll) or one that had already rejected failed every such request with `500 trap`, and the handler's own writes rolled back. Rust's `after` work had the same flaw (a panic before its first wait), and a failure after the node took the answer could still cut the body of a client slow to read it (the node aborts a body when the instance dies).
- **Reproduced:** yes: `review2_wait_until_that_never_settles_fails_the_answered_request` and `review2_wait_until_rejected_before_the_answer_fails_the_answered_request` (js.rs) got `500 trap` with storage `{}`; `after_work_that_panics_leaves_the_answer_standing` (integration.rs) got `500 trap` for `/panic-at-once`.
- **Fix:** the node resolves the future `response.new` returns (WASI's "result of transmission") once the client has read the answer whole or gone, not when the request body was consumed. The SDK starts `after` work only then, and the JavaScript runtime runs its waitUntil settling as `after` work, so its abort comes only then. One model for both, in README "Commits".
- **Status:** fixed in 2031f79 and c65cfb3 (`fix/r3-dev`)

### N-711: waitUntil work's writes before the answer commit with it, even if it later rejects (round-3 review 2, D4 finding 2)
- **Where:** `js/runtime` (waitUntil semantics)
- **Scenario:** a waitUntil promise's work starts when it is made, so what it writes while the handler runs is in the journal when the answer commits, and stays committed when the work rejects afterwards. The same holds for a Rust streamed body's writer, which starts as the handler returns: what it writes before the node has taken the answer commits with it.
- **Status:** by design, documented in 2031f79 (README "Commits", js/README.md): the answer commits everything written up to it. The node has one journal per request, not one per task, so the only alternatives were holding back the answer until all waitUntil work ends (which defeats waitUntil) or failing the answer after it was sent. `review2_wait_until_that_wrote_before_the_answer_commits_that_write_when_it_rejects` now asserts this (and that the work's write after the answer rolls back).

### N-712: a waitUntil promise registered after the answer was never awaited or checked (round-3 review 2, D4 finding 3)
- **Where:** `js/runtime/src/prelude.js` (`answer`)
- **Scenario:** `answer` took `Promise.all` of the promises registered by then; one that waitUntil work registered later was neither awaited (dropped if it outlived the rest) nor checked (its rejection ignored, its write committed). Rust's `after` called from `after` work was queued for the next request instead of running in this one.
- **Reproduced:** yes: `review2_wait_until_registered_after_the_answer_commits_its_write_when_it_rejects` (`{"nested":1}` committed), `wait_until_registered_after_the_answer_is_waited_for`, `after_work_passed_on_by_after_work_runs_before_the_next_request`.
- **Fix:** the runtime waits until the set is empty (`settled`: `Promise.all`, again while more were registered); `after` from work after the answer starts it at once, so it is part of the same post-return stretch.
- **Status:** fixed in 2031f79 (`fix/r3-dev`)

### N-713: a broken upstream stream reached the gateway's client looking whole (round-3 review 2, D3)
- **Where:** `sdk/rust/src/lib.rs` (`Response::stream`), `examples/gateway`
- **Scenario:** `Streaming::next` reports an upstream cut, but a streamed response's writer returned `()` and its trailers were always `Ok(None)`, so the gateway's client got 3 tokens and a clean end.
- **Reproduced:** yes: `review2_gateway_tells_its_client_when_the_upstream_breaks_off` (examples.rs).
- **Fix:** the writer returns `Result<()>`; on `Err` the SDK ends the body with an error in its trailers (the node then fails the body, so the client sees it break off), and once the answer is out panics as `after` work, so what the writer changed rolls back, as on a panic. The gateway's writer uses `?` on `next()`; `events`, `files` and `site` return `Ok(())`. This changes `Response::stream`'s signature (no compatibility path, AGENTS.md rule 1).
- **Status:** fixed in 551d13d (`fix/r3-dev`)

### N-714: the oauth example's session cookie was a plain cookie over https (round-3 review 2, D1)
- **Where:** `examples/oauth/src/lib.rs`
- **Scenario:** a sibling host could set `session=<its own valid session>; Domain=<parent>; Path=/u-<its login>`, and a victim's browser was signed in as the attacker on the attacker's pages: login CSRF without the callback.
- **Reproduced:** yes: `review2_oauth_session_cookie_is_a_host_cookie_over_https` (a planted plain `session` cookie got `200`).
- **Fix:** over https the session cookie is `__Host-session` (Secure, Path=/, no Domain), as the nonce cookie already was; logout clears it with the same attributes.
- **Status:** fixed in a50b695 (`fix/r3-dev`)

### N-715: a guest that traps while its answer commits gets a 500, though the commit may land
- **Where:** `object.rs` (`serve`: `commit(acc).await` runs while the store's event loop goes on)
- **Scenario:** found while fixing N-710. After the guest returns its answer, the node commits it on a blocking thread and the guest's other tasks keep running. If one traps before the commit ends, the store's run ends, the reply still pending gets `500 trap`, and the commit thread (which owns the volume until it ends, S1) still lands the writes. The SDKs no longer fail work after the answer before it is out (N-710), so this needs a guest that traps on its own (a raw `spawn_local` task that panics at once). The same holds for a request deadline that passes during the commit (S1).
- **Reproduced:** from source; not run.
- **Suggestion:** treat it as the existing ambiguous-outcome case ("Do not interpret every error as a rollback", README "Commits"), or have the node finish the answer's commit before reporting the trap.
- **Status:** open

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
- **Status (2026-11-13, `beta/recovery`):** backup is continuous shipping to `capture.store`; restore of one object (to its head or an earlier version) or of an app's objects runs in a running node through the operator socket (`nanovm admin … restore`, `versions`, `backlog`), authorized as root on the node's host (B-500). The offline restic backup below is gone. Remaining: retention in the store (N-166), auto-restore on a fresh node (B-505), deletion (`beta/objects`). Earlier: offline backup/restore done in `fab908c` with validated main follow-through. Restic snapshots retain journals under app admission locks; only complete snapshots are tagged, and restore requires an exact ID and new destination. Real jailed SQL/file/KV crash recovery into a fresh root passes unprivileged and as root, including another write/restart. Six mutations caught; full suite, ten SQL tests, Python CLI tests, demo, benchmark and adversarial review pass. Host runtime unchanged at 5,085; tooling adds 89 lines. README defines offline authorization, ownership/deployment records, retention guidance and old-writer fencing. Deletion API, live snapshots, automated retention and actual off-node deployment remain open; no active file ownership after main integration.

### W-6: JS and Python guests
- **What:** guests built with componentize-js and componentize-py, running the same endpoints as `counter`, to prove the host for other languages.
- **Status:** JavaScript done on `cf/onramp` with our own QuickJS runtime (`js/`; componentize-js has no WASIp3 HTTP yet), `env.kv` on the composed provider, nine tests in `tests/js.rs`. Python open.

### W-7: `nanovm publish`
- **What:** one command that pushes an app with `oras` (layout as in `publish-guests.yml`) and signs it keylessly (`cosign` with a browser sign-in, or a CI attestation).
- **Status:** done on `cf/onramp` (e74577c): `nanovm publish` pushes the same layout itself (with oci-client; since `cf/oci-push`, nanovm's own `registry::Oci`), checks the component as a node will first, and prints the signing steps and TXT record; `.github/workflows/publish-app.yml` is the manual attestation template. Signing stays outside the CLI (W-102).

### W-8: benchmarks against workerd and Spin
- **What:** density (memory per object), cold starts and warm latency against workerd (Durable Objects) and Spin, on the same machine, with the method written down.
- **Status:** open

### W-9: ordered committed-state transfer and recovery
- **What:** capture immutable committed content and namespace state before later mutations, transfer ordered versions and recover without the original root. The undo journal is not a replication feed. Survey Litestream, LiteFS and Durable Objects before choosing machinery.
- **Status:** in progress; bounded local capture done in `b7bf806` and included in main integration. Pending publication recovers before further writes. Local acknowledgement remains unchanged; remote receipt, recovery authority, retention and failover policy remain unresolved. Offline backup is complete, not this work's objective. No active file claim after local capture delivery.
- **Evidence:** example tests and separate executable drill pass: ordered/idempotent receipt, immutable response/post-return versions, exact file/namespace recovery after source deletion, rejected mutation rollback, receiver process-exit cuts and continued local writes after recovery. Four guard-removal mutations fail and are restored. An unsent locally accepted write is deliberately absent on recovery. Same-host roots/processes do not establish remote durability; full local suite retains N-32 failures. Review found no blocker for the disclosed research scope, not production integration.
- **Runtime candidate evidence:** streaming TAR snapshots share the journal decision; 14 commit/publication and 28 rollback sync-error/process-exit cuts pass. Response/post-return separation, jailed-worker restart, bounds and SQL/file/KV recovery without the original root pass. Four mutations (discard, publication recovery, byte cap, version cap) fail and are restored. Review found missing recovery durability barriers; two regressions fail before correction and pass after, with no remaining review blocker. Full serial suite passes 105 unit/69 integration/two proof tests, plus all 11 SQL tests; this passing run does not fix historical N-32 intermittency. No physical-power-loss or remote-durability claim.
- **Delivery:** bounded proof done in `310c992`, CI 42438468359 passed; runtime capture done in `b7bf806`, CI 42491417145 passed. Restored full suite/SQL checks and demo pass; measured full-copy write cost and noisy default benchmark controls are recorded in HANDOFF. Verify exact-head main integration CI. No production receiver, retention/GC, restore API or promotion authority is delivered by local capture.

### W-10: groundwork for many nodes
- **What:** a lease hook before instantiation in `table.rs` (always granted on one node), an epoch carried by each commit, request ids so the proxy can retry safely. Placement belongs in the proxy.
- **Status:** done on `cf/multinode` (multinode agent): the proxy places objects on `nodes` by rendezvous hashing and follows 421s; nodes take leases with rising epochs from an object store (conditional writes; `object_store`, S3 and compatibles), renew one session record each, refuse and evict what they lost, and refuse commits without the lease (`Node::lease_epoch`); calls between objects go through the proxy with their callers. No request ids are needed: the proxy retries only refusals that ran nothing. Failover, a lost node's late commit and cross-node calls are tested; see `notes/multinode.md`. The epoch is not yet part of W-9's committed versions (W-90), and data does not move between nodes until W-9's stream does.

### W-11: run requests that don't write in parallel
- **What:** stateless routes on any node, and read-only requests on read replicas of an object. On one node, a reader beside the writer would see uncommitted data (the undo journal writes in place), so replicas need committed state: a snapshot, or W-9's change records. Whether a request writes isn't known in advance, so routes would be declared read-only, with `/data` mounted read-only to enforce it.
- **Status:** open (research)

### W-12: trusted caller header on calls between objects
- **What:** the node sets `nanovm-caller: <id>` on calls and strips it from client requests, so an object can serve internal endpoints only to its app's objects. Left out of the first version because testing it needs a guest that echoes headers (a guest change; see W-1's note on publishing).
- **Status:** done with W-602 on `cf/multinode-r3`, as `nanovm-callers` (the chain, caller last) rather than `nanovm-caller`

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

- **Status:** open (exploration requested by owner)

### W-35: bound what the proxy buffers for objects across all apps
- **What:** each event answer and each publish is collected whole in the proxy, up to `max_response_body_kb` (8 MiB), with no bound across sockets, objects or apps: 1,000 sockets of one app answered at once is 8 GiB, and publishes are as many as objects committing at once. One app's plain Wasm can so exhaust the proxy every app shares. Parsing is not the problem: an answer is parsed and applied within one poll, so its parse (about 6–14 times its size, `ws::parse`) is bounded by the proxy's threads, and publishes now parse in about their size (N-72). Found by the sec review.
- **Design:** (1) collect an answer up to `socket_backlog_kb`, not `max_response_body_kb` (in `Proxy::socket`'s `deliver`): `apply` queues an answer's messages at once, each costing its length plus 64, so an answer bigger than the socket's backlog overflows it and drops the socket anyway (only a flood of subscribe controls would fit). Answers then cost at most one backlog per socket, within the per-socket budgets the proxy already has. (2) publishes: a proxy-wide semaphore of publish bytes in flight (an operator setting, or `max_sockets_per_app × socket_backlog_kb` per app), acquired for the declared or collected size before buffering, so one app's commits wait rather than grow the proxy.
- **Status:** open

### W-100: publish nanovm-sdk to crates.io
- **What:** apps depend on the SDK by git today. Its bindings read WIT from `../../components/keyvalue/wit` and `../../wit/config`, which a git checkout has and a crates.io package would not: vendor the two WIT packages into `sdk/rust/wit` (with Spin's license) before publishing, and pin `wasip3` with nodes' wasmtime.
- **Status:** open

### W-101: compose without the `wac` CLI
- **What:** `tools/build-app.sh` needs `wac` 0.12 (`cargo install`, about a minute) only for apps that use key-value. A prebuilt, digest-pinned provider published with each release, or a small composer on the `wac-graph` library outside the runtime crate, would drop that install. Composing inside `nanovm run` was rejected: `publish` and nodes need the composed component, so it belongs in the build (notes/onramp.md).
- **Status:** open

### W-102: test the laptop signing path end to end
- **What:** `nanovm publish` prints `cosign attest --new-bundle-format …` for signing without CI. The node accepts Sigstore bundles attached as OCI referrers over the manifest digest (as GitHub attestations are); whether cosign's new-bundle attestation lands where `registry::verify` looks, with a subject it accepts, is untested here (no cosign, and nothing may be published to a real registry). Verify against a throwaway public repository, then document the exact command, or replace it with what works.
- **Status:** open

### W-103: warn when a published app can't be pulled anonymously
- **What:** nodes pull anonymously, and a new ghcr.io package is private, so a first publish looks fine and then every node refuses it. After pushing, `nanovm publish` could read the manifest back anonymously with `registry::Oci` and say "make the package public" when that fails.
- **Status:** open

### W-104: move the test guests onto the SDK
- **What:** `guests/counter` and `guests/hostile` still use `guests/util`. Porting them shrinks them by about a third (cf/optimistic-feynman's D1), but any change under `guests/**` republishes them from main, which needs the owner's approval.
- **Status:** open

### W-600: let a request roll back and still choose its answer
- **What:** a handler that returns an error rolls back everything the request did, and the client gets `500` with `x-nanovm-error: guest-error`; any answer it gives instead, whatever its status, commits. So a checkout whose card is declined after it has written (`examples/shop`) can only say `500`, never `402`, and an app that wants a clean `409` must check everything before its first write. A way to answer *and* roll back (for example a reserved response header the host strips, which marks the request failed but sends the response) would let the SDK offer `Response::rollback()`. It touches the commit path in `object.rs`, so it is for its owners.
- **Status:** open

### W-601: let an app serve `/`
- **What:** every URL's first segment names an object, so `/` is `400 bad-object-id` and a site has to live at `/www/` (`examples/site`). A per-app default object for paths that don't name one (or for `/` only), set in the manifest, would let a domain's root be a site. It changes routing in `server.rs`/`proxy.rs`, so it is for their owners.
- **Status:** open

### W-602: tell an object who is calling it
- **What:** a call between objects (`http://nanovm/objects/{id}/…`) reaches its target as an ordinary request, so an object can't tell its app's own calls from a client's, and every route meant for other objects is public (`examples/tictactoe`'s `/join`, `examples/chat`'s `/rooms/…`); apps must write them defensively. The host could set a header naming the calling object on calls and strip it from every request that enters from outside (the node's front door and the worker link), so an object could keep internal routes. It is a security property of every entry path (`server.rs`, `egress.rs`, the worker link), so it is for their owners.
- **Status:** done on `cf/multinode-r3` (with W-10): calls carry `nanovm-callers` (the objects waiting on the call, the caller last), which only nodes set: the calling worker overwrites a guest's own, the proxy drops a client's and keeps only the node's on its private call route between nodes. `callers_are_named_by_nodes_only` (one node behind the proxy) and `nodes::callers_are_named_by_nodes_only_across_nodes` check that a client's forged header never reaches the guest and a call shows its caller; each of the three rules is mutation-checked. README: "The callee knows its caller". A node reached without the proxy trusts what it is sent, as for `nanovm-app`.

### W-90: carry W-10's epoch in W-9's committed versions, fenced at the store
- **What:** name each committed version `(app, object, epoch, seq)` from `Node::lease_epoch` (the line before `quota.commit()` in `object::commit`), and make the write that makes a version durable off the node conditional on the object's lease record (`If-Match` the version the grant, or the last commit, produced), so a node that lost the lease mid-commit can't publish a version. A taker restores up to the head it found. Contract in `notes/multinode.md`.
- **Status:** done on `fix/lease-r3fix` (round-3 review M1–M5, S3): versions carry the lease epoch (`cf/storage-r3`) and are put once at `<app>/<object>/<seq>-<sha256>.tar`; a version counts only once the object's head (`<app>/<object>/head`, `{sequence, digest, node, lease}`) names it, and every head write is conditional on the version read. Before a worker gets a lease, its supervisor claims the head at the lease's epoch (`ship::claim`), so a node with an earlier lease can't move it however late its upload lands (`review_takeover_during_upload_fences_the_old_writer`); the shipper moves it only under this node's live lease, once claimed, to a version that follows it (`review_shipping_requires_a_live_matching_epoch_not_just_a_node_name`); `remote_ack` and restore follow the head. The claim makes the node's copy the head's, restoring it if need be, so a takeover never serves older state (`nodes::review_failover_with_separate_disks_preserves_remote_acknowledged_state`); with leases a store is required and acknowledgement defaults to remote. The head lives in the capture store rather than the lease record (notes/multinode.md says why). Open: remote retention (N-166).

### W-91: keep a released lease for a while
- **What:** an evicted object's node keeps its lease (it is in the node's session anyway) for a grace period after the instance goes, then releases it, so a cold start within the period skips the store (N-183). Meanwhile another node gets 421 and the proxy sends the request back to the holder, which is the object's home anyway unless the node list changed.
- **Status:** open (proposal)

### W-80: W-9 production capture, shipping and restore
- **Done (bc8f45a, cf/storage):** crash-atomic capture of each committed version at both commit points as changes against the version before (listings, whole new or moved entries, byte ranges), snapshots every `snapshot_every`; a bounded local outbox (`max_versions`, `max_mb`: full means writes are refused); a put-once `Store` (directory, S3-compatible with SigV4 + `If-None-Match: *`); `remote_ack` (off by default: the owner hasn't chosen an RPO); an epoch and a fence object as the hook for W-10's leases; `ship::restore` on a fresh root.
- **Open:** remote retention (N-166; the head is the snapshot pointer); a shipper notification (N-165); a `nanovm restore` command. Decided and done on `fix/lease-r3fix`: acknowledgement is remote by default with leases, local on one node; shipping is fenced by the head (W-90).
