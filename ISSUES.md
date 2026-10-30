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
- **Status:** fixed with N-13 (e410377): a worker holds at most `response_max` per link it is allowed

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
- **Status:** open: not fixed on purpose. At the default `max_sockets` of 1,000 per app, the scan costs a few microseconds against a publish's HTTP hop, and indexing by object would add code to every open, close and test. Worth it only if `max_sockets` grows by an order of magnitude.

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
- **Status:** fixing

### N-23: a dev-mode node behind the proxy is reachable from the public
- **Where:** `config.rs` (`dev_on_loopback`)
- **Scenario:** dev mode requires a loopback `listen`, but a node behind the proxy listens on loopback anyway (the proxy's default `node` is `127.0.0.1:8080`). A dev-mode node there runs unsigned apps with private egress for anyone the proxy routes.
- **Reproduced:** no (phase 7 review)
- **Fix:** refuse dev mode in release builds (`cfg!(debug_assertions)` or a cargo feature), or document that it holds only while nothing forwards to the node.
- **Status:** open

### N-24: the proxy (and the node) exit on an accept error, and the proxy has no handshake or header timeouts
- **Where:** `proxy.rs` (`serve`: `listener.accept().await?`, `tls.accept`), `server.rs:30`
- **Scenario:** at the fd limit, `accept` fails with EMFILE and `serve` returns, ending the process; about 1,024 idle TCP connections do it at a default soft limit. `tls.accept` has no timeout, and without a hyper timer there is no header-read timeout.
- **Reproduced:** no (phase 7 review)
- **Fix:** log, sleep about 10 ms and continue on accept errors; `timeout(10 s)` around the TLS accept; a `TokioTimer` and a header-read timeout on the proxy's connections.
- **Status:** open

### N-25: the proxy's caches are unbounded and not single-flight
- **Where:** `proxy.rs:48-49` (`routes`, `digests`), `route`, `digest`
- **Scenario:** every distinct `Host` costs a DNS-over-HTTPS query and an entry that is never removed (a wildcard record makes it free); errors aren't cached, so a bad record is asked about on every request, inviting the resolver to rate-limit the proxy; a popular host's expiry sends a burst of identical queries.
- **Reproduced:** no (phase 7 review)
- **Fix:** drop expired entries past a size limit when inserting; cache errors briefly; one lookup in flight per host.
- **Status:** open

### N-26: a running app's accepted policies grow without bound
- **Where:** `supervisor.rs:101`, `supervisor.rs:213-218` (`Live.policies`)
- **Scenario:** each distinct `(trust, min)` a request names is re-verified over the network (holding the app's start lock) and kept once accepted; varying `min=` or adding junk `trust=` entries grows the set and costs a referrers fetch each time.
- **Reproduced:** no (phase 7 review)
- **Fix:** clear the set past about 16 entries.
- **Status:** open

### N-27: links are capped per worker only, and high
- **Where:** `link.rs:192-201`, `supervisor.rs:328`
- **Scenario:** the N-13 cap is `max_live_objects × (max_outbound_concurrency + 1)` per worker, 5,000 by default, with nothing across the node. A worker that got out of Wasm can hold 5,000 to 10,000 supervisor fds (links plus upstreams); past the supervisor's fd limit, its accept loop fails and the node exits (N-24).
- **Reproduced:** no (phase 7 review)
- **Fix:** a node-wide semaphore shared by every `link::serve`, sized from the fd limit, or a much smaller per-worker cap.
- **Status:** open

### N-28: a drain doesn't wait for the old worker to be gone
- **Where:** `supervisor.rs:519-525` (`drain`)
- **Scenario:** `drain` drops the kill sender but doesn't wait for the process to exit, so the next worker can start on the same volumes a moment before the old one is dead.
- **Reproduced:** no (phase 7 review; predates phase 7)
- **Status:** open

### N-29 (efficiency): uid lookup and chown run on the front-end thread
- **Where:** `supervisor.rs` (`uid`, `chown_tree`)
- **Scenario:** blocking filesystem walks on the single-threaded front end stall every request on that thread while a worker starts.
- **Reproduced:** no (phase 7 review; predates phase 7)
- **Status:** open

### N-30: the proxy doesn't strip hop-by-hop headers
- **Where:** `proxy.rs` (`forward`)
- **Scenario:** `Connection`-listed and other hop-by-hop headers from the client are forwarded to the node. Low risk (the node is HTTP/1.1 and strips its own headers), but a proxy should drop them.
- **Reproduced:** no (phase 7 review)
- **Status:** open

### N-31 (removable): code the phase 7 review found removable
- **What:** `check_digest` in `AppRef::from_headers` repeats the one in `fetch`, which runs before any path (1 line); TLS in the proxy (`proxy.rs` `tls`, `tls_cert`/`tls_key`, about 30 lines) if a terminator in front is assumed (owner's call: there is no ACME anyway); depending on `oci-spec` directly instead of oci-client, which the runtime now uses only for its re-exports (fewer dependencies, not lines).
- **Status:** open

### N-32 (tests): two integration tests failed once on a cold first run
- **Where:** `tests/integration.rs:1335` (`registry_app_switches_versions_after_a_drain`, first request not 200), `tests/integration.rs:1515` (`the_journal_is_out_of_reach_and_works_from_a_relative_data_dir`, killed for using more than 100 ms of CPU)
- **Scenario:** in the first full run after rebuilding, at 7ea5e8e, both failed; alone, and in two more full runs, they passed. Likely load while components compile (the CPU budget is tight for a first request on a busy machine) plus network for the ghcr.io test, but the cause isn't confirmed.
- **Reproduced:** once, not on demand
- **Status:** open

## Planned work

Agreed with the owner on 2026-10-30; PLAN.md's "Roadmap" has the order and the reasons, HANDOFF.md the decisions (Q41–Q51).

### W-1: per-object SQLite demo (priority)
- **What:** a guest where every object is its own SQLite database on `/data`, committed with its response, and a demo that shows it (for example a room or a ledger per object, with a crash mid-request rolled back). The showcase for nanovm.
- **Where:** `guests/` (a new guest), `demo/`, README. Adding or changing guests triggers `publish-guests.yml` on push to `main`, which publishes new versions: ask the owner first.
- **Status:** open

### W-2: OpenTelemetry by default; deprecate the stats and events APIs (high priority)
- **What:** OTel traces (a span per request, with queue wait, instantiation, guest time, commit, calls), metrics (what `/_nanovm/stats` reports today) and guest logs (each object's stdout and stderr), on by default and tagged with app, object and version. Then remove `/_nanovm/stats` and `/_nanovm/events`.
- **Design notes:** a jailed worker can't open sockets, so its telemetry has to leave through the supervisor, over the egress link or its stderr; the supervisor exports OTLP to an operator-configured collector. Look at the `opentelemetry` and `tracing-opentelemetry` crates before designing. Touches the supervisor, so coordinate with the rewrite.
- **Where:** `metrics.rs`, `object.rs`, `server.rs`, `supervisor.rs`, `demo/demo.sh` (it reads `/_nanovm/stats`).
- **Status:** open

### W-3: move WebSockets into the front proxy
- **What:** the proxy holds client sockets, Pushpin-style, and sends each event to the object as the plain `POST` it gets today; publishes go to the proxy. Most of `ws.rs` leaves the node, and the node only answers requests. Resolves N-14 and N-20. Supersedes Q31.
- **Where:** `proxy.rs` (on branch `proxy`), `ws.rs`, `supervisor.rs`, the WebSocket tests.
- **Status:** open, after the proxy lands

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

