# Issues

One list of known problems (`N-n`) and planned work (`W-n`), so nothing is done twice or missed. Before starting one, set its status to `fixing` (or `in progress`), with who has it, and push; when it is on `main`, set `fixed in <commit>` (or `done in <commit>`) in the same push. New problems get the next `N-` number, new work the next `W-` number.

- **Reproduced**: shown against the real code or the OS. **From source**: traced through the code, not run.
- Status: `open`, `fixing`, `fixed in <commit>`, or `deferred: <why>`.

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

### N-32 (tests): two integration tests failed once on a cold first run
- **Where:** `tests/integration.rs:1335` (`registry_app_switches_versions_after_a_drain`, first request not 200), `tests/integration.rs:1515` (`the_journal_is_out_of_reach_and_works_from_a_relative_data_dir`, killed for using more than 100 ms of CPU)
- **Scenario:** in the first full run after rebuilding, at 7ea5e8e, both failed (and at add9b68, `data_quota_survives_restart`, `tests/integration.rs:438`, an in-process test); alone, and in two more full runs, they passed. Likely load while components compile (the CPU budget is tight for a first request on a busy machine) plus network for the ghcr.io test, but the cause isn't confirmed.
- **Reproduced:** once, not on demand
- **Status:** partly fixed in d64c80a: the in-process quota tests failed when the full suite loaded the machine enough that a 4 MiB write passed `hostile`'s 100 ms CPU budget (wall time inside guest polls; the journal test's failure said so). They now use a `filler()` app with 1 s, as `file_count_limit` already did. The ghcr.io test's one failure is unexplained (likely network); open for that. Still seen once after that: `a_failed_requests_writes_are_rolled_back` (`tests/integration.rs:1498`, its first `/commit` on `hostile`) on the first full run after a rebuild, at the N-23 commit; the commit tests could use `filler()` too, but their `then=spin` case expects a CPU kill, so check the status it gets with a 1 s budget. Seen again at the W-15 commit in `the_journal_is_out_of_reach_and_works_from_a_relative_data_dir` (`tests/integration.rs:1520`); the commit and journal tests are now the ones that flake.

### N-33: initial proxy DNS lookups have no deadline
- **Where:** `proxy.rs`, `forward` → `resolve` → `lookup`
- **Scenario:** a resolver that accepts but never answers holds the initial HTTP request or WebSocket handshake before socket admission. W-3's event-delivery timeout does not cover that initial lookup. Predates W-3; found by its adversarial review.
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
- **Where:** W-2 branch, worker SDK export queue and supervisor relay; not yet merged into main
- **Reproduced:** oha 1.16.0 sent three 200,000-request runs through the real supervisor/jailed-worker path to OpenTelemetry Collector 0.164.0. All HTTP requests succeeded at 35–39k/s, but only 406,006/600,000 request spans reached the Collector. Its accepted/sent counts matched and refusal/failure counts were zero; the worker reported a full SDK queue. Even a 20k/s rate-limited run lost 2,439/200,000 spans.
- **Experiments:** 512-span batches reduced loss but did not eliminate it; two front-end threads also still lost spans. The SDK's experimental async processor with two concurrent exports delivered all 600,000 at 28–33k/s, but failed the strengthened flush barrier test. These experiments are not retained; do not claim the earlier 75–79k/s in-process result establishes production-path capacity.
- **Latency investigation:** mean worker export round trip 14.86ms, of which stamping was 0.68ms; Collector HTTP await was 9.40ms including scheduling. Lowering Tokio's I/O event interval did not help and was reverted. Stable concurrency 64 delivered 200,000/200,000 spans at 21,833 requests/s; concurrency 256 delivered 166,452/200,000 at 26,112/s. Next distinguish supervisor scheduling delay from Collector service time. These are session-specific observations, not a guaranteed capacity.
- **Scheduling fix:** the CLI now creates the supervisor on a dedicated one-thread control runtime; startup tasks use its captured Handle rather than the originating frontend. Relay/reaper/export I/O no longer competes with requests on frontend zero. Measured worker export round trips fell from 12.6 to 3.5 ms; the same front-thread-one workload that lost spans delivered all spans in three runs afterward. The frontend-lifetime regression fails when startup uses the old runtime. No custom exporter or queue/batch/sampling change.
- **Sustained validation:** 2,000,000 HTTP200 requests at 25,282/s, p99 28.6 ms, delivered 4,000,000/4,000,000 spans to official Collector 0.164.0 with derived metrics enabled; accepted/sent counts match and refusals are zero. Three preceding 200k runs also delivered all 400k spans each. This repairs the reproduced loss, not all overload/outage loss. The requested 64k HTTP target remains unresolved; historical main's confirmed 64k figure is in-process, while its HTTP rate is 27–29k on this orb. See HANDOFF for matched controls and CPU/timing evidence.
- **Status:** reproduced scheduling-induced loss fixed in W-2; keep W-2's performance/production acceptance open. Bounded best-effort export remains independent of request success; no lossless-accounting or universal capacity promise.

### N-37: final post-return commit failures lose diagnostic classification
- **Where:** `object.rs`, the final `commit(acc).await.is_err()` in `serve`
- **From source:** after sending a successful response, a failed final commit marks the request span as error but discards the I/O error and supplies no `error.type` or failing-phase classification. The initial commit has a `commit-failed` classification; the final one should be equally diagnosable without changing the already-sent HTTP status.
- **Status:** fixed in the focused W-2 continuation: retain the I/O result, record `post-return-commit-failed` and a bounded correlated diagnostic, preserve HTTP status. A gated test-only component outside `guests/**` proves actual post-return calls/logs and fails the second commit by moving its journal log. The regression failed before the fix; the full locked suite passes. Published guests remain unchanged.

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
- **Status:** fixed in 98c1698: a head is at most 64 KiB at every hop (hyper's `max_header_size`, which also bounds trailers; over it a client gets 431), a guest's map at most 64 KiB, and an instance holds at most `memory_mb / 64 KiB` resources (`max_resources` is the lower of the two: 1,024 at 64 MiB, 256 at the counter's 16 MiB). So header maps the host holds for an instance stay within its memory limit, and 64 KiB is twice what nginx accepts by default (four 8 KiB header buffers). `guest_header_maps_are_bounded_by_the_memory_limit` (a test-only probe route) fails without each part.

### N-90: references accepted non-ASCII tags
- **Where:** `proxy.rs` `parse` (a `_nanovm` record's `app=`), through oci-spec's `Reference`
- **Scenario:** oci-spec's reference regex matches tags with Unicode `\w`, so `app=ghcr.io/org/app:é` parsed, although the OCI distribution spec defines a tag as `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`. Harmless (no registry serves such a tag, and the proxy answered 502 either way), but the grammar and the spec disagreed. Found by the lean pass's differential test.
- **Reproduced:** yes (unit test against oci-spec 0.10)
- **Status:** fixed in d485694: the runtime parses references itself, with ASCII tags

### N-91 (tests): `a_node_runs_at_most_max_apps` assumes a request reaches its worker within 200 ms
- **Where:** `tests/integration.rs`, `a_node_runs_at_most_max_apps` (the `busy(c, 600)` then `sleep(200 ms)` step)
- **Scenario:** the test spawns a slow request to app `c`, sleeps 200 ms, and expects `c` to be busy when app `a` asks for a slot. At load average 8 to 20 (several agents compiling on 4 cores) the spawned request has not reached the worker yet, `c` looks idle, `a` takes its slot and gets 200 instead of the expected 502.
- **Reproduced:** yes: fails alone, every time, at load average 18 to 20 on the unchanged `491e790` and on `cf/lean`; passes on an idle machine
- **Fix:** wait until the worker reports the request in flight (or the app's `Live` is held by a request) instead of sleeping a fixed time
- **Status:** fixed in e27f59a (lean2): the test waits for the worker's snapshot to show the request `handling`, and its first slow request (2.5 s, request timeout 10 s) outlasts the next app's start. On two cores shared with four busy loops the old test failed 3/3, the new one passed 3/3; on one core with three loops both fail, since a start can still outlast the fixed-length request.

### N-92 (tests): `the_proxy_outlasts_idle_and_excess_connections` binds the default publish port
- **Where:** `tests/integration.rs`, `the_proxy_outlasts_idle_and_excess_connections`
- **Scenario:** the proxies the test starts get a free `listen` port but no `publish_listen`, so each binds the default `127.0.0.1:8444`. Any other proxy on the host (a demo, another checkout's test run) makes the test fail with `Address already in use`.
- **Reproduced:** yes, while another agent's tests ran
- **Status:** fixed in dd2efa4 (the test gives each proxy a free publish port)

### N-50 (efficiency): every instance made and removed its own `/tmp` directory
- **Where:** `object.rs`, `new_store` and `TmpDir`
- **Scenario:** each instance did `mkdir` for its scratch directory and `remove_dir_all` when it ended, plus a `canonicalize` of its volume (10 `readlink`s here). On this VM an `rmdir` costs about 70 µs of CPU and, on an ext4 disk mounted with `discard`, 3.3 ms of wall time, spent on the tokio worker that drops the store. Churn (new object plus LRU eviction) and every restart after a trap paid it.
- **Reproduced:** yes (strace, perf, and a mkdir/rmdir microbenchmark; see `notes/cold.md`)
- **Status:** fixed in f1e89d6 (empty scratch directories are reused, a used one is removed; the objects directory is resolved once). Churn 1,388 → 3,359/s, restart after a trap p50 0.8 → 0.3 ms.

### N-51 (density): 30 KB of every live object's heap was our own padding
- **Where:** `table.rs` (`Entry::tx`), `object.rs` (`charged`, `commit`)
- **Scenario:** a tokio channel allocates 32 slots at creation and a `Job` is 592 bytes, so each mailbox was 19 KB. The actor task was 16.9 KB: `charged` held its 7.6 KB future twice (moved in, then pinned), and `commit`'s supervisor publish path sat inline.
- **Reproduced:** yes (heaptrack peak consumers over 2,000 idle objects; `-Zprint-type-sizes`)
- **Status:** fixed in d0e85be: 116 → 89 KB RSS and 67.6 → 35.7 KB heap per live object; the actor task is 2.9 KB, guarded by `an_actor_is_small`.

### N-52 (efficiency): cold starts' remaining host costs
- **Where:** `object.rs` `new_store`, `journal.rs` `local_at`, wasmtime
- **From a profile** (perf, 3,000 cold starts after N-50/N-51): `mkdir` is still the biggest single cost, about 20% of cold-start CPU, because a brand-new object needs its volume and (while no scratch directory is free) a scratch directory. Here it is inflated by ext4's `recently_deleted` inode scan after the benchmark deletes the previous run's data. Page faults are about 11%; `Journal::local_at` (a `statx` and a `readlink` of `/proc/self/fd` for every open in `/data`, read-only ones included) about 3%; the `/data` usage walk 2%. A new object still costs 2 fds and 4 mappings.
- **Also:** of the 35.7 KB heap an idle object keeps, wasmtime's own instance metadata is about 25 KB (15.7 KB for the counter's 4 core instances, plus its vectors and `Store`). Each idle store also keeps 2 fiber stacks resident (16 KB), and its tables 8 KB; wasmtime 50 has no public way to hand a store's cached fiber stack back to the pool.
- **Status:** open; `local_at` belongs to the journal and request path (lean/perf), the rest to wasmtime upstream.

### N-60: in-process nodes can't publish to WebSockets
- **Where:** `egress.rs` (`send_request`, the `PUBLISH_HOST` branch), `object.rs` `commit`
- **Reproduced:** yes. A node built with `Node::new(.., None)` (tests, the benchmark, any embedder, and now `nanovm run`) held publishes only for `Route::Supervisor`; a publish from an in-process node went to its egress policy and was denied (`HttpRequestDenied`, 502 to the guest). WebSockets could only be tested through a supervisor and proxy.
- **Fix:** every node holds publishes until its request commits; an in-process node then delivers them to its own `Node::sockets` (the proxy's `ws::Sockets`). `run::nanovm_run_serves_requests_sockets_and_publishes` and `only_well_formed_publishes_are_held` fail without it.
- **Status:** fixed on `cf/dx`

### N-61: a publish to the wrong `http://nanovm/` path is accepted and dropped
- **Where:** `egress.rs` `hold`
- **Reproduced:** from source and test: the worker answered 200 to a POST to any `http://nanovm/<path>` with a well-formed body and held it; at commit the supervisor answered 404 for anything but `/publish`, and nothing told the guest.
- **Fix:** `hold` answers 404 at once for other paths (the supervisor keeps its own check, since it cannot trust a worker). `only_well_formed_publishes_are_held` fails without it.
- **Status:** fixed on `cf/dx`

### N-62 (tests): `sqlite_repeated_updates_remain_bounded` fails on a loaded host
- **Where:** `tests/integration.rs` (`--ignored`, mandatory in CI)
- **Reproduced:** yes, 4/4 alternating runs with the baseline (`491e790`) and the SDK-ported `p3.wasm` alike, at load average 8–13 on 4 cores: 1,000 updates in one request exceed the 10 s CPU budget, which is wall time inside guest polls including inline disk syscalls. Passed earlier on an idle machine (CI). Same class as N-32.
- **Status:** open (environmental; consider measuring guest CPU with thread CPU time, or a larger budget for this test)

### N-63: a node with no Collector prints two export errors a second
- **Where:** `telemetry.rs` (SDK exporters), `main.rs` (stderr at WARN)
- **Reproduced:** yes: `nanovm serve` (or an in-process node) with nothing on `telemetry_endpoint` logs `Export.Failed.Exhausted` and `BatchLogProcessor.ExportError` every second, because infrastructure snapshots are exported every second whether or not anything happened. A developer's first impression of the node is a wall of errors.
- **Fix in `run`:** `nanovm run` serves its own loopback OTLP sink and prints requests and guest output instead. `serve` is unchanged: rate-limit or deduplicate export-failure warnings, or skip snapshots that repeat the last one.
- **Status:** open for `serve`

### N-64 (tests): `a_node_runs_at_most_max_apps` is timing-sensitive
- **Where:** `tests/integration.rs`, the max-apps test (the refusal check near line 2865)
- **Reproduced:** yes, on `cf/dx` it failed 5 of 7 runs on its own, with the baseline and the SDK `hostile.wasm` alike, including on an idle machine (load 1.8). It gets 200 where it expects the 502 refusal: `b`'s 900 ms busy request has already finished, because starting `c`'s worker takes most of that time, so `b` is idle and `a` takes its slot. Not A/B'd against a baseline runtime, but the branch doesn't touch worker scheduling.
- **Fix:** hold the busy requests open until the refusal has been checked (for example a gate the test releases), not for fixed sleeps.
- **Status:** open

### N-110 (efficiency): opening a key-value bucket creates its directories, even to read
- **Where:** `kv.rs` `open_bucket` (creates `.kv` and `.kv/<bucket>`, journaled)
- **Reproduced:** the JS counter's first `GET` on a new object (`env.kv.get`, nothing written) pays a journaled create and a commit: cold-start p50 25–36 ms against ~1 ms for a JS app that doesn't touch kv (js-bench, cf/js). Each new object also spends two of its `max_files` on empty directories.
- **Fix:** create the directories on the first write; a missing bucket reads as empty and lists nothing.
- **Status:** fixed in ad0ccaa (lean2): JS counter cold first GET p50 32–40 ms → 1.3–1.4 ms (js-bench, 3 alternating pairs). Fixing it exposed N-130.

### N-111: JavaScript apps share std's hash seed, fixed at build time
- **Where:** `crates/nanovm/examples/js-wizer.rs` (pre-initialization gives `wasi:random/insecure-seed` a constant)
- **From source:** Boa keys its shape-transition maps with std's `RandomState`, seeded once per instance. Pre-initialization snapshots that seed, so every instance of every JS app has the same keys, readable from the published component anyway. An attacker who can make an object build objects with chosen property names (`JSON.parse` of a body) could aim for collisions; the cost lands on that object's own requests, within `cpu_ms`. Low.
- **Status:** fixed on cf/qjs: Boa is gone (W-50). The build-time std seed now keys only rquickjs's internal map from Rust types to prototypes, never app data. QuickJS hashes atoms and shapes without a seed and xors its Map/Set seed onto finished hashes, so no seed separates colliding keys there in any QuickJS embedding (N-141).

### N-112: wasmtime-wizer 50.0.0 writes an invalid component when it drops the init export
- **Where:** upstream `wasmtime-wizer` component rewrite
- **Reproduced:** with the default (`keep_init_func` false), the snapshot of a `wasm-component-ld` component fails validation: "core instance 29 has no export named `wizer-initialize`": the core export is removed, the component's alias to it is not.
- **Fix:** `js-wizer` keeps the export (`keep_init_func(true)`); calling it again is a no-op. Report upstream.
- **Status:** worked around in cf/js

### N-40: the supervisor's readiness probe leaves a link connection in front thread 0's pool, driven by the control runtime
- **Where:** `supervisor.rs` (`start`: `live.link.send(get("/_nanovm/ready"))` runs on the control runtime), `link.rs` (`Dialer::pool` picks the pool by the `THREAD` thread-local, which is 0 on control threads too)
- **Scenario:** found by the perf review at `491e790`. The probe's socket is registered with the control runtime's reactor and then pooled as thread 0's. Front thread 0 reuses it, so each request on it is woken through the control thread: in some supervisor instances the control thread made 2 voluntary context switches per request at c=1 (0.03 in others, depending on pool order).
- **Reproduced:** yes (per-thread `voluntary_ctxt_switches` deltas over 3,000 requests at c=1)
- **Fix (proposed):** give control threads their own pool index (`Dialer::new(requests, threads + 1)` and `link::set_thread(threads)` on the control runtime's threads), or don't pool the probe's connection.
- **Status:** fixed in e88acd5 (hop; the control runtime's threads use the pool after the front threads'; on a fresh supervisor at c=1 the control thread went from 1.90 to 0.03 voluntary context switches per request)

### N-41: a read on a link wakes the other process for nothing
- **Where:** `link.rs` (tokio registers every `UnixStream` for readable and writable)
- **Scenario:** found by the perf review. Unix stream sockets wake writers waiting for space (`unix_write_space`) every time the peer consumes data, unlike TCP, which only does so after the writer ran out of space. With writable interest always registered, the worker's read of each request wakes the supervisor's epoll thread, and the supervisor's read of each response wakes the worker's: one spurious wakeup per hop per request at low concurrency.
- **Reproduced:** yes (`perf record -e sched:sched_wakeup` on the worker: one wakeup per request from the link read)
- **Fix (proposed):** register link sockets for readable only (`AsyncFd` with `Interest::READABLE`) and wait for writability only while a write is blocked.
- **Status:** fixed in fb7055d (hop; a write that finds the socket full waits on a dup of the descriptor registered for writes)

### N-42: at low concurrency the worker wakes its idle runtime threads for every request
- **Where:** `supervisor.rs` (`worker_main`'s multi-threaded runtime), `object.rs` (the actor's store poll)
- **Scenario:** found by the perf review. At c=1 the worker made about 4 voluntary context switches per request (one per runtime thread) where 1 is needed: the actor wakes itself during its own poll (Wasmtime yields by self-wake, guest events wake it), Tokio queues it for stealing and wakes an idle thread, and a thread that finds work after searching wakes another. Per-request CPU at c=1 is about 3.5× that at c256. A waker that re-polls the actor on a self-wake did not remove them.
- **Reproduced:** yes (per-thread context-switch deltas, strace and `sched_wakeup` traces)
- **Cause (hop):** two things woke the actor during its own poll, and Tokio treats that as a yield (back of the queue, wake an idle thread to steal it). An object idle for over a tick had an expired epoch deadline, so its next request yielded at its first instruction; and Wasmtime wakes the store's task three times per request (the guest returning, its task ending, once in its event loop).
- **Status:** fixed in 1b6b882 (the deadline is reset when a request starts) and 4c37fad (a wake during the store's own poll polls again at once; the epoch callback's yield, and any wake after a tick, still reach Tokio). At c=1 the worker now makes about 1.1 voluntary context switches per request (was 4-5); see notes/hop.md

### N-43: the supervisor redoes pure per-app work on every request
- **Where:** `registry.rs` (`AppRef::name`: SHA-256 and six allocations; `from_headers`: clones the cached `Reference`, parses the trust list)
- **Scenario:** found by the perf review: about 4.7% of the supervisor front thread at c256. Both results are pure functions of the headers, cacheable like the reference parse (Q70), never the authorization.
- **Status:** open (small; below the 2% bar for the whole request)

### N-44: every guest open under `/data` read `/proc/self/fd` to locate the volume
- **Where:** `journal.rs` (`local_at` → `fd_path`)
- **Scenario:** found by the perf review: a `readlink` per open was 4.5% of in-process CPU (the counter's GET opens `/data/count`).
- **Status:** fixed in be6ea35 (the volume's own descriptor resolves without asking; subdirectories still do)
### N-130: a touched path under what became a file fails the commit
- **Where:** `journal.rs` `sync_if_there` (commit and recovery)
- **Scenario:** a request makes a directory inside a missing one (the stock call fails, but the journal has already recorded the path as touched), then makes a file where the missing directory would be. The commit syncs every touched path and skipped only `NotFound`; this one fails with `ENOTDIR`, so the commit failed and the request rolled back. A guest could only fail its own request this way; the hostile guest's kv `plant` did it once buckets stopped being made at open (N-110).
- **Reproduced:** yes (`a_path_under_what_became_a_file_commits`, which fails without the fix)
- **Status:** fixed in 62c5104 (lean2)

### N-131: nanovm doesn't compile on macOS
- **Where:** `link.rs` (`RecvFlags::CMSG_CLOEXEC` in `recv_fd`, `SendFlags::NOSIGNAL` in `send_fd`); also check the macOS branches of `metrics::rss_bytes` and `journal::fd_path`
- **Scenario:** rustix 1.1.6 defines `MSG_CMSG_CLOEXEC` and `MSG_NOSIGNAL` only for Linux-like targets (`cfg(not(any(apple, ...)))`), so the crate fails to build on macOS, although PLAN.md says the core and the supervisor/worker split run there, and `nanovm run` on a developer's Mac is likely how people first try nanovm.
- **Reproduced:** from source (rustix's `cfg`s); no macOS machine here
- **Fix:** on Apple, set `FD_CLOEXEC` on the received fd after `recvmsg`, and use `SO_NOSIGPIPE` on the socketpairs instead of `MSG_NOSIGNAL`; then build and run the suite on a Mac (the rss/fd_path branches have never been compiled since). Owner decides whether macOS stays supported.
- **Status:** open

### N-132: a store stops counting as live before its scratch directory is released
- **Where:** `object.rs` (`impl Drop for ObjState` decrements `stores_live`; the `_tmp: TmpDir` field drops after it)
- **Scenario:** `stores_live == 0` (what `Node::shutdown` and tests wait for) can be seen while the last instance's `/tmp` directory is still being handed back or removed. `only_empty_scratch_is_reused` failed once this way in a full run at load ~12 (`["80", "81"]`).
- **Reproduced:** yes, once (the test now waits for the directory itself, 6cf4798)
- **Fix:** uncount the store after its fields drop (e.g. drop `_tmp` first in `Drop`, or a last field whose `Drop` uncounts). `object.rs` was the hop branch's this round.
- **Status:** open (test fixed)

### N-150 (efficiency): a restarted worker removed every scratch directory its predecessor left
- **Where:** `object.rs` (`Scratch::new`)
- **Scenario:** found by the cold2 workstream. A worker that is killed, retired or switched leaves one `/tmp` directory per live instance (N-50 keeps them for reuse). The next worker's start removed them all before serving, at about 3 ms of wall time each on a disk mounted with `discard` (N-50): its first request waited 0.1–0.6 s after 200 objects and 1.0–1.9 s after 1,000.
- **Reproduced:** yes (phase marks in the worker; `notes/cold2.md`)
- **Status:** fixed in e082357 (empty ones are handed out again, used ones removed): first request after 1,000 objects 1,211 → 51 ms

### N-151 (efficiency): every app start spawned a process and built its engine on the request's path
- **Where:** `supervisor.rs` (`start`, `worker_main`), wasmtime's pooling allocator
- **Scenario:** found by the cold2 workstream (W-29). A start (an idle app retired for another, a crash, a version switch) forked the supervisor, exec'd the 49 MB binary (dynamic relocation and glibc start-up: 3.4 ms of CPU) and built the engine: 7 ms, nearly all of it wasmtime creating the stack pool, one `mprotect` per stack slot (`2 × (max_live_objects + 32)` of them, so it grows with the node). Of a 21 ms restart, 11 ms was this, none of it app-specific.
- **Reproduced:** yes (phase marks and perf; `notes/cold2.md`)
- **Status:** fixed in 99d0f55 (a worker is spawned ahead, builds its engine from the node's config, and waits for its app before its jail): restart 21.5 → 10.0 ms, LRU retirement 34 → 18 ms

### N-140: runaway recursion in JavaScript is a trap, not a RangeError
- **Where:** QuickJS (rquickjs-sys 0.14): under `__wasi__` its stack limit is compiled out (`update_stack_limit` sets none)
- **Reproduced:** `const f = (n) => f(n + 1) + 1` fails its request with `500 trap` ("wasm trap: call stack exhausted", wasmtime's `max_wasm_stack`) and the instance is replaced; the app can't catch it as it could a `RangeError` in a browser. Test `limits_kill_runaway_javascript` covers it.
- **Status:** accepted (bounded, rolled back like any trap); a catchable error needs a QuickJS stack check that works under WASI (the shadow stack pointer is known; upstream sets no limit)

### N-141 (low): QuickJS hashes can be flooded with chosen keys
- **Where:** QuickJS atoms and shapes (no seed); Map/Set (`map_hash_key` xors the context's seed onto the finished hash, which never separates two keys that collide)
- **From source:** an app that puts attacker-chosen strings into objects or Maps (`JSON.parse` of a body) can be fed keys that share a bucket; the cost lands on that object's own requests, within `cpu_ms`. True of every QuickJS embedding.
- **Status:** accepted

### N-142: QuickJS reads the clock when a context starts, which pre-initialization forbids
- **Where:** `JS_NewContextRaw` seeds `Math.random` and the Map seed from `gettimeofday`; `JS_AddPerformance` reads the monotonic clock
- **Reproduced:** with rquickjs's stock context, pre-initialization traps. The runtime defines `gettimeofday` itself: it answers 0 only while the context starts (any later build-time read traps, so top-level `Date.now()` still fails the build), and replaces `Math.random` with a generator seeded from the host at first use; it leaves out `performance` (W-65). Test `instances_share_nothing_from_the_snapshot` fails without the `Math.random` replacement (mutation-checked).
- **Status:** worked around on cf/qjs

### N-143: js-bench's "KB/object" counted shared memory-image pages once per instance
- **Where:** `crates/nanovm/examples/js-bench.rs` (RSS delta over the cold phase)
- **Reproduced:** each instance maps the component's memory image from the `.cwasm` file; a read fault maps up to 64 KiB of page cache around it (fault-around), and RSS counts those shared pages again for every instance. From `/proc/self/smaps` over 200 live objects: Boa 1220 KB RSS per object, of which 277 KB private; QuickJS 580 KB, 222 KB private (plus ~80 KB of anonymous memory each, as for Rust).
- **Status:** fixed on cf/qjs (js-bench also reports private memory per object)
### N-152 (tests): the jail test jailed one thread of a two-threaded process
- **Where:** `tests/integration.rs` (`jail_blocks_escapes`, `jail_probe`), `jail.rs`
- **Scenario:** found by the cold2 workstream. The uid, Landlock and seccomp bind only the calling thread and threads it starts later, so the jail must be entered with one thread (it said so in a comment). The probe called `Jail::enter` from a libtest test thread, so the harness's main thread stayed unjailed: the probes ran on the jailed thread and passed, but a jail entered with a thread too many would not have failed the test. Production was unaffected: `worker_main` jails before starting any thread.
- **Reproduced:** yes (`Jail::enter` with a thread count check refused the probe: 2 threads)
- **Status:** fixed in 99d0f55 (the jail refuses a process with more than one thread; the probe forks a single-threaded child, which its test stops and continues through the process group)

### N-153 (efficiency): every worker start received and hashed its whole component
- **Where:** `supervisor.rs` (`start`), `registry.rs` (`load`), `engine.rs` (`load_component`)
- **Scenario:** found by the cold2 workstream. The supervisor read the cached component and piped it to the worker, which SHA-256'd it to find its cached compile, though the registry had checked the bytes when it fetched them. For the 5.4 MB JavaScript component that was 40 ms of worker CPU and 7.5 ms of supervisor CPU per restart (no SHA extensions on this VM's CPU).
- **Reproduced:** yes (perf, phase marks)
- **Status:** fixed in 77497df (the worker gets the digest and the cache file, opens it before its jail, and reads it only to compile): JavaScript restart 78 → 16 ms

### N-154 (efficiency): a switch waits 5–12 ms for the old worker's address space to be torn down
- **Where:** `supervisor.rs` (`drain` waits for `gone`), wasmtime's stack pool
- **Scenario:** found by the cold2 workstream. After the kill, the process only reads as gone once the kernel has unmapped it, and a worker has about 4,500 mappings (two per pool stack slot, N-151) plus its instances' memory: dropping an engine alone took 4.5 ms in a probe. It is now the largest part of a switch to a cached version (about 10 of 21 ms), and of an LRU retirement. Waiting for full exit is what guarantees the old worker can no longer touch the volumes, so this is kept; fewer mappings would need wasmtime to stop splitting the stack pool per slot (W-71).
- **Reproduced:** yes (phase marks around `drain`)
- **Status:** open

### N-155 (efficiency): a worker's telemetry set-up touches its whole span queue
- **Where:** `telemetry.rs` (`Telemetry::new`: `BatchSpanProcessor` with `max_queue_size(2048)`)
- **Scenario:** found by the cold2 workstream (perf over 30 restarts). The SDK's bounded channel initializes all 2,048 `SpanData` slots up front, so creating it page-faults its memory in: most of the 0.7–1.0 ms `Telemetry::new` takes at each worker start, and memory every worker keeps. A smaller queue risks dropped spans under load (N-36); not changed here.
- **Reproduced:** yes (perf: page faults under `mpmc::array::Channel::with_capacity` from `BatchSpanProcessor::new`)
- **Status:** open

### N-120: a heavy request with a free lane never gave up its thread
- **Where:** `object.rs` (the epoch callback's `YieldCustom` for requests past `HEAVY_AFTER_NS`)
- **Scenario:** found by hop. Past 10 ms of CPU a request hands back its heavy lane at every tick and takes it again. When a lane was free, taking it completed at once, so the request never returned to the executor between 10 ms and its CPU budget. Work stealing hid most of it (other threads take the tasks queued behind it), but not the task in Tokio's LIFO slot, and nothing on a one-thread runtime.
- **Reproduced:** yes (`a_spinner_yields_its_thread_at_each_tick`: a short request beside a spinner on a one-thread runtime finished at 102.8 ms, with the spinner's CPU kill)
- **Status:** fixed in 4805ba9 (the heavy path yields once before taking a lane)

### N-121: a jailed worker drops request spans above roughly 25k spans per second
- **Where:** the worker's `BatchSpanProcessor` (queue of 2048) exporting through the supervisor's relay
- **Scenario:** found by hop. With oha at 256 clients on the 4-core VM (other agents loading it), runs at 20-28k req/s lost 0-11% of the worker's `request` spans ("BatchSpanProcessor dropped a Span due to queue full"); supervisor spans were all delivered. At a fixed 15k req/s nothing was lost, before or after hop's changes. The export path (one export at a time, through the link, the relay's three slots and the Python counting sink) is slower than the request path at that rate; whether the sink or the relay is the limit was not measured.
- **Reproduced:** yes, in both the wave-1 build and hop's
- **Status:** fixed: the worker's telemetry export runs on a single-threaded runtime of its own, with its own pool of egress links, so a batch never queues behind guest work (the supervisor isolates its control runtime the same way, N-36). Idle-machine HTTP A/B, oha c256, 100 objects, 3 alternating rounds: request spans delivered 94.4–99.6% → 100% in every run, with throughput 28.9k → 28.6k req/s, p99 19.8 → 18.9 ms and CPU 108 → 107 µs/request (baseline 26.1k, 30.1 ms, 120 µs).

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
- **Status:** source beta/example and host recovery fixes pushed on the validation branch through [74615d9](https://github.com/carsonfarmer/nanovm/commit/74615d9588dd415694dac6a3910531368800a136); CI passed. Not merged to main or published as a package.

### W-2: OpenTelemetry by default; deprecate the stats and events APIs (high priority)

**Complete (Q71):** owner delegates deployment to engineering. Recommend a separately budgeted standalone/gateway Collector, or dedicated physical cores on the same host. The measured 2.17% application overhead satisfies that deployment's gate; 7.58% colocated overhead and 13.62–14.70 µs/request additional Collector CPU remain explicit costs. Runtime 5,315 versus baseline 4,955 (+360, 7.3%). Correctness, complete exports, full suites, mutations, production check, demo, quick benchmark and CI pass. No guest changes or LOC-only refactor. Remote-machine and durable-backend performance are not tested; operators must validate their backend/topology. Earlier pending-decision checkpoints below are superseded; proceed with validated fast-forward delivery.

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

### W-3: move WebSockets into the front proxy
- **What:** the proxy holds client sockets, Pushpin-style, and sends each event to the object as the plain `POST` it gets today; publishes go to the proxy. Most of `ws.rs` leaves the node, and the node only answers requests. Resolves N-14 and N-20. Supersedes Q31.
- **Where:** `proxy.rs`, `ws.rs`, `supervisor.rs`, `egress.rs` (`hold`), `object.rs` (`commit`), the WebSocket tests.
- **Status:** done in e62e9ac (migration), bb8a418 (cleanup). Runtime 4,247 baseline → 4,264 implementation → 4,246 cleanup. Full suite passed twice: 93 unit, 64 integration, one ignored; seven mutations caught and restored cleanly. Demo: 70,000 normal requests succeeded, 100 attacks contained, canary zero failures. Quick benchmark has mixed small variation and unchanged 96 KB/object density; HANDOFF records numbers and limitations. Adversarial review found no confirmed runtime blocker; identity test gaps fixed, N-33/N-34 recorded. Guests unchanged.

### W-4: alarms, outside the node
- **What:** a scheduler beside the proxy. An object sets an alarm with a response header; when it fires, the scheduler sends the object a request. Durable across restarts of the scheduler; the node is unchanged.
- **Status:** open, soon after W-3

### W-5: data lifecycle (planning)
- **What:** a plan for deleting an app's or an object's data, exporting a volume, backups and retention, and how each is authorized (likely through the proxy).
- **Status:** open

### W-6: JS and Python guests
- **What:** guests built with componentize-js and componentize-py, running the same endpoints as `counter`, to prove the host for other languages.
- **Status:** open

### W-7: `nanovm publish`
- **What:** one command that pushes an app with `oras` (layout as in `publish-guests.yml`) and signs it keylessly (`cosign` with a browser sign-in, or a CI attestation).
- **Status:** open

### W-8: benchmarks against workerd and Spin
- **What:** density (memory per object), cold starts and warm latency against workerd (Durable Objects) and Spin, on the same machine, with the method written down.
- **Status:** open

### W-9: research: change records shipped off the node
- **What:** whether a commit can emit a self-contained record of what it changed (the journal already knows the touched paths) and ship it to replicated storage, Litestream-style, so a node is a cache of its volumes. Survey Litestream, LiteFS and Durable Objects' storage design first.
- **Status:** open

### W-10: groundwork for many nodes
- **What:** a lease hook before instantiation in `table.rs` (always granted on one node), an epoch carried by each commit, request ids so the proxy can retry safely. Placement belongs in the proxy.
- **Status:** open, after W-3

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
- **Goal:** reduce capability-specific host code while retaining the existing `wasi:keyvalue` and `wasi:config` interfaces. Research/prototype only; do not remove the current providers or publish replacements yet.
- **Key-value:** move encoding, lookup, listing, batch and atomic operations from `crates/nanovm/src/kv.rs` into a composed Wasm component over the existing filesystem. Preserve the file layout, quotas, memory bounds, error behavior, atomic operations, and mixed SQL/file/KV request rollback. Compare removable host code against component size, memory, cold starts and latency; do not introduce SQLite as a prerequisite for KV.
- **Config, if feasible:** move parsing/lookup from the `wasmtime_wasi_config` host integration into Wasm. Explore an existing capability for read-only deployment inputs (for example a shared read-only mount or environment-based injection), preserving app scope, immutability, absent-key behavior and exposure rules. Do not copy config into each object's writable volume or bake deployment secrets into published Wasm. The host still owns supplying inputs and enforcing access.
- **Composition gate:** synchronous WIT calls cannot block on async P3 filesystem I/O, as W-1 demonstrated. Establish a valid standard-interface/ABI strategy or preload mechanism first; fewer host handlers must not come at the cost of a custom backchannel, a quota bypass or more lifecycle machinery. Report separately whether KV and config genuinely reduce host coupling.
- **Acceptance:** run the existing KV/config integration contracts against composed providers, including KV quota/rollback cases and config isolation; inventory the host code removed and the bootstrap responsibilities retained. Compare against the current implementation and recommend keep/move independently for each.
- **Status:** open (exploration requested by owner)

### W-35: bound what the proxy buffers for objects across all apps
- **What:** each event answer and each publish is collected whole in the proxy, up to `max_response_body_kb` (8 MiB), with no bound across sockets, objects or apps: 1,000 sockets of one app answered at once is 8 GiB, and publishes are as many as objects committing at once. One app's plain Wasm can so exhaust the proxy every app shares. Parsing is not the problem: an answer is parsed and applied within one poll, so its parse (about 6–14 times its size, `ws::parse`) is bounded by the proxy's threads, and publishes now parse in about their size (N-72). Found by the sec review.
- **Design:** (1) collect an answer up to `socket_backlog_kb`, not `max_response_body_kb` (in `Proxy::socket`'s `deliver`): `apply` queues an answer's messages at once, each costing its length plus 64, so an answer bigger than the socket's backlog overflows it and drops the socket anyway (only a flood of subscribe controls would fit). Answers then cost at most one backlog per socket, within the per-socket budgets the proxy already has. (2) publishes: a proxy-wide semaphore of publish bytes in flight (an operator setting, or `max_sockets_per_app × socket_backlog_kb` per app), acquired for the declared or collected size before buffering, so one app's commits wait rather than grow the proxy.
- **Status:** open

### W-40: drop `parking_lot` for std's `Mutex`, and name tokio's features
- **What:** the runtime uses `parking_lot::Mutex` only for its non-poisoning `lock()`, and tokio's `full` feature also turns on tokio's own `parking_lot` use. With std's `Mutex` (futex-based since Rust 1.62) and tokio's features listed (`rt`, `rt-multi-thread`, `net`, `time`, `sync`, `macros`, `process`, `io-util`), `parking_lot`, `parking_lot_core` and `lock_api` leave the build (3 crates; measured with `cargo tree`). Every `.lock()` in `object.rs`, `supervisor.rs`, `quota.rs`, `egress.rs`, `ws.rs`, `proxy.rs` and `table.rs` changes, so it waits for the branches on those files to land. Take locks through one small helper, `.lock().unwrap_or_else(PoisonError::into_inner)`, so a panic while holding one behaves as today. Needs an A/B of CPU per request: tokio's scheduler and timer locks change too.
- **Status:** open. lean2 measured it: 265 → 262 crates (`parking_lot`, `parking_lot_core`, `lock_api`), with tokio's features `rt-multi-thread, net, time, sync, macros, process, io-util, fs` and `parking_lot` kept as a dev-dependency for the tests. Not done in wave 2: half the lock sites are in files the hop branch was rewriting, a non-poisoning wrapper adds about 10 runtime lines, and tokio's own locks change, so it needs the HTTP A/B of CPU per request.

### W-41: one helper for the egress spans and the permit-holding body
- **What:** in `egress.rs`, the `http.client` path (`send_request`) and `call` each set the same `http.request.method` and `url.path` attributes with the same truncation, and each wraps the response body in the same `map_frame(move |f| { let _ = &permit; f })` to tie the outbound permit to the body. One function for each would remove about ten lines and keep the two paths from drifting.
- **Status:** done in 760b3e0 and 238bf82 (lean2): `telemetry::request`, `egress::holding`, `egress::to_guest`, `egress::verdict`.

### W-42: one TLS trust store for the node's own clients
- **What:** guest egress verifies servers against the compiled-in `webpki-roots`, while the registry client and the proxy's DNS-over-HTTPS and registry calls use reqwest's `rustls` feature, which verifies against the host's certificate store through `rustls-platform-verifier`. Giving those clients a rustls `ClientConfig` built from `webpki-roots` (reqwest's `tls_backend_preconfigured`, as `egress::TLS` does) would make every TLS decision independent of the host's `/etc/ssl` and drop `rustls-platform-verifier`, `rustls-native-certs` and `openssl-probe`. It stops honouring CAs an operator installed, so it is the owner's call; registries must be on public addresses anyway.
- **Measured (lean2):** it drops no crate. `aws-lc-rs` stays because `sigstore-crypto` (signature verification) depends on it directly, and `rustls-platform-verifier` comes with reqwest's `rustls-no-provider` feature too, so only the trust decision would change.
- **Status:** open (owner decision; no dependency or size win)

### W-43: decide which wasm proposals guests may use
- **What:** wasmtime's `gc`, `threads` and `stack-switching` cargo features decide whether guests may use GC types and exceptions, shared memories and stack switching. They are on because they are wasmtime's defaults, not by decision; the lean pass kept them, since turning one off changes which components load. If the node doesn't want them, dropping them removes their runtime code from every worker binary (GC heaps are allocated lazily, so there is no reservation to win back). Measure binary size and cold starts before deciding.
- **Status:** open (owner decision)

### W-25: shard object volume directories
- **What:** every volume is a direct child of `objects/<app>`, so each new object's `mkdir` takes that one directory's lock and adds a link to it (N-35's link limit). Sharding (for example `objects/<app>/<2 hex chars>/<id>`) fixes both. It moves existing volumes, so it needs the owner's decision (no compatibility code). Scratch directories don't need it: they are reused (N-50).
- **Status:** open (idea, not measured)

### W-26: reuse warm memory slots without page faults (`pagemap_scan`)
- **What:** on Linux ≥ 6.7, `PoolingAllocationConfig::pagemap_scan(Enabled::Auto)` with `linear_memory_keep_resident` / `table_keep_resident` resets only an instance's dirty pages in place instead of `madvise`-ing them away, so the next instance in that slot takes no page faults and no TLB shootdown (5% of cold-start CPU here). It helps churn, restarts and idle objects coming back, not first-ever cold starts (new slots). Cost: warm unused slots keep their dirty pages resident. Also try `decommit_batch_size`.
- **Status:** open: read wasmtime 50's source, not measured (wrap-up came first). Measure with `bench --only churn,kill` and RSS after churn. cold2 did not get to it either; note that in a jailed worker the `PAGEMAP_SCAN` ioctl would need a seccomp rule, and wasmtime probes it when the pool is created, which a spare worker now does before its jail.

### W-27: remember an object's `/data` usage across its instances
- **What:** skip the usage walk at instance start by keeping each object's committed usage in the node. Analysis: the counters mix `/data` and `/tmp` (an instance's `/tmp` is discarded), any drift in the counters would then persist instead of being corrected at every start, and a failed request's rollback changes the usage. The walk is 5 syscalls for the counter guest (2% of cold-start CPU) but O(files) for an object with many KV keys. Not adopted: not worth weakening exact accounting until a real workload shows the walk.
- **Status:** deferred: measured cheap for typical volumes

### W-28: pre-initialized guests (`wasmtime-wizer`)
- **What:** `wasmtime-wizer` 50.0.1 snapshots a component after its init function. Measured with a scratch in-process program (8 slots, 64 objects each re-instantiated 4 times): the counter guest costs 235–356 µs CPU for a re-instantiation plus first GET against 43–100 µs warm; the composed SQLite example 1.69 ms against 0.66–1.0 ms warm (it opens its database on every request). Its start functions are under 1% of cold-start CPU, so for Rust guests there is little to snapshot; the SQLite gap (about 0.7–1 ms) is the place to try. It is a guest build step (dx), not runtime code, and a snapshot must not share secret state (RNG seeds) between objects.
- **Status:** open (measured the gap, not the snapshot)

### W-29: worker cold start
- **What:** not measured: process spawn, jail, reading the component, `.cwasm` deserialize, and the pool's creation (it `mprotect`s a guard page per stack slot, `2 × (max_live_objects + 32)` calls) before the first request after a worker (re)start. Measure supervisor spawn to `/_nanovm/ready`.
- **Status:** done by cold2: measured phase by phase (`notes/cold2.md`); N-150, N-151, N-153 fixed, N-154, N-155 and W-70–W-73 open

### W-30: publish `nanovm-sdk` to crates.io
- **What:** today apps depend on it by git (`cargo add --git …/nanovm nanovm-sdk`). Publishing needs the `wit/keyvalue` and `wit/config` packages inside the crate (it reads `../../wit` now) and a version policy tied to `wasip3` 0.9 / WASI 0.3.0 until wasmtime hosts 0.3.2. Outward-facing: ask the owner.
- **Status:** open

### W-31: `nanovm run` restarts by draining, not crashing
- **What:** a rebuild re-executes `run`, so requests in flight roll back and sockets close (the page reconnects). Draining in-flight requests first, or keeping sockets across the restart as the proxy keeps them across version switches, would make rebuilds invisible. Small only if it reuses the supervisor's drain; otherwise not worth it for development.
- **Status:** idea

### W-32: work after the response in the SDK
- **What:** the SDK buffers whole bodies and answers when the handler returns. Durable Objects' `waitUntil` (post-return work, bounded by `post_return_timeout_ms`) and streamed bodies still need raw `wasip3` (see the post-return test probe). A `Response::then(future)` and a streaming body would cover both.
- **Status:** idea

### W-50: QuickJS for JavaScript apps
- **What:** A/B `rquickjs` against Boa in `js/runtime` (same prelude). Boa costs ~6× the Rust guest's CPU per warm request (253–298 µs against 38–48 µs), ~1.3 MB per live object and a 5.4 MB component; QuickJS is usually several times faster and much smaller. It needs a wasi-sdk sysroot, which CI already pins (wasi-sdk-34, for SQLite). Keep Boa unless the numbers say otherwise.
- **Status:** done on cf/qjs: QuickJS won on every measure (notes/qjs.md) and replaced Boa.

### W-51: share the JavaScript runtime's bindings with the guest SDK
- **What:** `js/runtime/src/lib.rs` duplicates request reading, response writing and the kv/config bindings of `guests/util`; use the dx workstream's guest SDK once it lands.
- **Status:** done on cf/qjs: the runtime is an SDK app (`serve!`, `Request`, `Response`, `fetch`, `kv`, `config`); the SDK gained `Request::url()`.

### W-52: JavaScript API gaps
- **What:** streaming bodies (`ReadableStream`), more than one module (bundling with esbuild works today), kv values beyond JSON (structured clone, bytes), batch get/put, a `/data` file API, alarms (W-4), and a supervised end-to-end test of a JS chat room (`env.publish` needs the supervisor).
- **Status:** open

### W-53: build JavaScript apps without a Rust toolchain
- **What:** today each app relinks the runtime with its source compiled in (~1 min, needs Rust). Pass the source to the pre-initialization call instead, so one prebuilt runtime plus `js-wizer` packs any app in seconds; make it a `nanovm` build command once dx's CLI lands.
- **Status:** open

### W-54: memory per JavaScript object
- **What:** a live JS object costs ~1.2 MB more than a Rust one. Boa writes to the header of every heap object it touches (reference counts), so serving a request copies much of the pre-initialized heap. Measure which pages a request dirties; compare with W-50.
- **Status:** measured on cf/qjs: most of the 1.2 MB was RSS counting shared pages (N-143); privately a Boa object cost ~350 KB, a QuickJS one ~250 KB, a Rust one ~50–80 KB. What is left is W-67.

### W-20: thread-per-core workers
- **What:** one current-thread runtime per core in the worker, each owning the objects that hash to it; the supervisor sends an object's requests on a link bound to that thread (the byte sent with each fd can name it). A request would then stay on one thread end to end in the worker, removing N-42's wakeups and the actor-to-connection hops.
- **Trade-off:** no work stealing between threads: hash collisions and CPU hogs (which yield every epoch tick) would delay the other objects on their thread, so "neighbours under attack" and `cpu_hogs_leave_cores_for_short_requests` must be re-measured. Needs an owner decision.
- **Status (hop):** not built. Its main motivation, N-42's wakeups, is fixed on the multi-threaded runtime (about 1.1 context switches per request in the worker at c=1), so what remains for it is cache locality under load. It would first need every tick to yield (N-120, done) and a way to keep heavy requests from delaying their thread's neighbours by a tick each.

### W-21: cheaper link framing
- **What:** hyper's HTTP/1.1 client in the supervisor and server in the worker cost about 5% of a request (excluding the syscalls, which any framing keeps). A length-prefixed framing could save about 4%, but every link user (requests, egress, telemetry relay, publishes, drains, restarts) would need it. Not worth it yet.

### W-22: telemetry still costs about 10% of a production request
- **What:** after N-74, the remaining cost is mostly the SDK: span creation and attributes, and OTLP encoding of the supervisor's own spans (about 3 µs per request) and the worker's (about 2.5 µs). Reducing it without a custom exporter (Q64/Q66) means fewer allocations per span (e.g. `Arc<str>` object ids instead of new `String`s).
### W-60: simplifications left in files another branch had (lean2)
- **What:** found in the lean2 pass, not done because the hop branch was changing these files:
  - `supervisor.rs`: the egress link's closure clones six captures per request; one small struct with a `serve` method holds them (about −4 lines). `admin()` and `get()` build empty bodies by hand (−3 with a shared helper).
  - `supervisor.rs` and `table.rs`: both snapshots start with the same process fields (pid, memory, uptime).
  - `object.rs`: N-132's ordering.
  - Done here as small, separate hunks: `forward` holds the worker through `egress::holding`, `verdict_ok` is `egress::verdict`, `dispatch` sets its span attributes with `telemetry::request`, `Worker` and `MemLimiter` take their zeroed fields from `Default`.
- **Status:** open

### W-61: `Supervisor::flush_telemetry` exists only for tests
- **What:** `flush_telemetry` (14 lines) and the worker's `/_nanovm/flush` route (5 lines in `server.rs`) are called only by the integration tests' `stats_json` and span checks, to read fresh snapshots without waiting for the 1 s export interval. Without them the tests would wait for the next periodic export (about 20 s more per suite run). Options: keep (test support in the runtime), or gate both behind the `dev` feature (out of production builds, still counted).
- **Status:** open (owner decision)

### W-65: `performance.now()` for JavaScript apps
- **What:** QuickJS's `performance` reads the clock when the context starts (N-142), so the runtime leaves it out. A prelude one over the host's monotonic clock, with the origin taken at the first call, would do; Workers freeze it during a request, which would be fine too.
- **Status:** idea

### W-66: streaming and the rest of W-52 on QuickJS
- **What:** W-52's gaps are unchanged by the engine switch (bodies are whole, one module, JSON kv values, no `/data` API, no alarms). QuickJS has async iteration and `Symbol.asyncIterator`, so `ReadableStream` over the SDK's body streams is now a prelude feature rather than an engine one.
- **Status:** idea

### W-67: less private memory per JavaScript object
- **What:** a QuickJS object's first request copies ~55 of the snapshot's ~150 heap pages (~220 KB, js-mem probe in notes/qjs.md): reference-count writes to the prelude's and the builtins' objects, spread over the whole heap, plus allocator metadata. A GC before the snapshot and `-O2` changed nothing. Ideas: lay the objects a request touches out together at build time (evaluate the prelude in a fresh arena), or keep QuickJS from writing to snapshot objects (immortal reference counts, an upstream change).
- **Status:** idea

### W-68: the per-request floor of a JavaScript app
- **What:** a minimal app (`return new Response("hi")`) costs ~330k instructions per GET against ~160k for the same component answering from Rust (valgrind, notes/qjs.md); the rest is QuickJS's call and object machinery for the prelude's Request/Response objects and rquickjs's conversions. Building the request object lazily from Rust, and the response without the `Headers` class when the app passes none, are the next cuts.
- **Status:** idea

### W-70: registry round trips at each worker start in production
- **What:** a start re-fetches the manifest and, outside dev mode, the referrers, each signature manifest and bundle, and checks them; `Oci::send` makes each fetch an anonymous request, a 401, a token request and a retry (tokens are not kept since the phase 7 review). For a signed app on ghcr.io that is about a dozen HTTP round trips per start, likely the largest part of a production app cold start; it could not be measured here (no outside network). Options, all owner decisions: keep a bearer token per (registry, repository) until it expires (bounded, unlike the map removed in phase 7); remember an accepted (digest, policy) across a worker's retirement for as long as a running worker remembers it (`Live::policies`), with the parsed manifest, so a restart of a known version needs no registry at all.
- **Status:** open (idea; request counts from source)

### W-71: wasmtime's stack pool: guard pages without one mapping each
- **What:** wasmtime 50 creates the pool's stack guards with one `mprotect` per slot at engine creation (7 ms for 2,264 slots here, N-151, now off the request path) and they cost about 4.5 ms to unmap at exit (N-154). Linux 6.13's `MADV_GUARD_INSTALL` makes guard pages without splitting the mapping. An upstream change (or lazily protected slots) would remove both costs.
- **Status:** open (upstream)

### W-72: compile time for a never-seen version
- **What:** compiling the 198 KB counter takes 370–450 ms of CPU (Cranelift: register allocation and e-graph passes), 160–450 ms of wall time on 4 shared cores; it is all of a never-seen digest's first request (a) and of a version switch (c). Faster settings (single-pass register allocation, no optimization, Winch) trade warm speed. Cranelift's incremental cache (wasmtime feature `incremental-cache`, a per-app `CacheStore` in the app's cache directory) could reuse unchanged functions across versions of an app. Not measured.
- **Status:** open (idea)

### W-73: start a switch's new worker while the old one exits
- **What:** a switch could hand the spare its app (jail, runtime, telemetry, loading the component: about 3 ms) while the old worker is torn down (N-154), holding back only what touches volumes and scratch directories until the old one is gone. Needs a second step in the worker's start; saves about 3 ms of a 21 ms cached switch.
- **Status:** open (idea)
