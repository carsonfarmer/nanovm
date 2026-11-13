# Security review (sec), from `491e790`

A fresh adversarial pass over every trust boundary. Five bugs fixed, each with a
hostile test that failed before the fix; one more measured and handed to the perf
agent; one piece of future work with a concrete design. Kernel 6.18 here offers
Landlock ABI 7.

## Fixed

| Issue | Boundary | What could happen | Fix | Proof |
|---|---|---|---|---|
| N-70 | worker → host | Code that escaped Wasm could send datagrams (and file descriptors) to any unix datagram socket on the host: journald, syslog, systemd's notify socket, abstract sockets. seccomp allowed `socketpair` of any type, and a datagram socket's `sendto` names its own destination. | `socketpair` for stream pairs only (one seccomp condition). | `jail_blocks_escapes`: the jailed probe reached a socket the test bound (`unix_datagram=ok`); now denied, and nothing arrives. |
| N-71 | client → proxy → object | A client could forge a WebSocket event, with a `Connection-Id` of its choosing, by sending the events content type second, in other case, with parameters, or comma-joined. The proxy checked only the first header, exactly. | Any `Content-Type` value (or comma-joined part) whose media type is the events type is refused. | `the_proxy_routes_hosts_by_dns`: the counter guest answered a forged `TEXT id` with `m:7` (the client's id); now 400. Unit test covers each spelling. |
| N-72 | guest → shared proxy | A guest's 8 MiB publish parsed as a JSON `Value` held 192 MiB and 200–370 ms in the proxy every app shares (and again in its worker). | Parse into typed structs; unread parts are skipped. | `publishes_parse_in_bounded_memory` (counts allocations): 1 MiB held 25 MB before; now under 1× for unread data, 5× for a flood of minimal items. |
| N-73 | worker → egress | A compromised worker could send an allowed CDN a `Host` the allowlist never named (domain fronting). | `Host` is set from the checked authority. | `the_upstream_hears_the_allowed_host`: the upstream heard `elsewhere.example` before. |
| N-75 (98c1698) | guest → worker memory | A guest could keep 10,000 `wasi:http` header maps of 128 KiB each (~1.25 GiB), or copies of a received ~400 KB head (~4 GiB), against a 64 MiB `memory_mb`. | A head is at most 64 KiB at every hop (hyper `max_header_size`), a guest's map at most 64 KiB, and an instance holds at most `memory_mb × 16` resources (256 at 16 MiB, 1,024 at 64 MiB). Later (N-691): a guest request's method, scheme, authority and path, which this left unbounded, count toward its 64 KiB head. | `guest_header_maps_are_bounded_by_the_memory_limit` (test-only probe route): at 32 MiB it held 600 maps of 60 KiB, new or copied, and a 100 KiB map; each part mutation-checked. |

Measurements for N-72 and N-74 used a counting allocator in a scratch crate over the
same crate versions (`serde_json` 1, `opentelemetry-proto` 0.33), on a busy machine:

```
publish as serde_json::Value: 8192 KiB -> peak 192 MiB (24x), 200-370 ms
publish typed:                8192 KiB -> peak 0 MiB, 21 ms
many items typed:             8192 KiB -> peak 42 MiB (5x), 108 ms
OTLP traces decode:           1023 KiB -> peak 198 MiB (198x), 146-270 ms
```

## Recorded, not fixed

- **N-74** (handed to the perf agent, who owns `telemetry.rs`): a compromised
  worker's 1 MiB telemetry batch of empty spans costs the supervisor 198 MiB and up
  to 270 ms of its single control thread. Proposed: stamp the resource at the wire
  level and drop the span-attribute scrub, or count submessages before decoding.
- **W-35:** nothing bounds what the proxy buffers for objects across sockets and
  apps (each answer or publish up to 8 MiB). Design in ISSUES: collect answers up
  to `socket_backlog_kb` (a larger answer overflows the socket's backlog anyway),
  and a proxy-wide semaphore of publish bytes in flight.
- **N-33** (already open) also covers the proxy's tag lookup, which a registry can
  trickle.

## Checked and found sound

**Jail (worker as compromised native code).**
- Signals: no `kill`, `tkill`, `rt_sigqueueinfo`, `pidfd_*`, `timer_create`; `tgkill`
  only to its own pid; `F_SETOWN`/`F_SETOWN_EX`/`F_SETSIG` refused; `ioctl` is
  `FIONBIO` only (no `FIOSETOWN`); `prctl` is thread names only (no
  `PR_SET_PDEATHSIG`). So Landlock's signal scope (ABI 6) would add nothing.
  Apps sharing the supervisor's uid cannot signal each other or it.
- Reaching other processes: no `socket`/`connect`/`bind`, no `ptrace`,
  `process_vm_*`, SysV or POSIX IPC, `pipe2`, `memfd_create`, `mknod`; Landlock
  allows only the app's own directories and `/proc/<own pid>` read-only (so not
  `/proc/<supervisor>/mem`; magic links like `/proc/self/root` resolve to paths
  outside the rules). With N-70 fixed, no unix socket is addressable either.
- `clone` needs `CLONE_THREAD` (the kernel refuses it with `CLONE_NEWUSER`/
  `CLONE_NEWPID`, and other namespaces need capabilities); `clone3` is ENOSYS.
  `prlimit64` cannot raise limits. Other architectures' syscalls are refused.
  `mmap`/`mprotect` refuse W+X at once; W→X transitions are allowed (Wasmtime
  needs them), which is not a containment boundary.
- Descriptors a worker sends on its egress control socket: any type it holds;
  a non-socket fails `epoll` registration and ends only its own link loop. Links
  are capped per worker and node-wide (N-13, N-27).
- Telemetry relay: identity is stamped by the supervisor; signals other than
  `traces`/`logs` are refused, so the collector path can't be chosen; bodies,
  slots and deadlines are bounded (N-74 aside).
- Publishes: the supervisor stamps the app; the worker can name only objects of
  its own app. Stderr lines are capped at 4 KiB (a worker can still write
  terminal control characters into the supervisor's log: cosmetic).

**Guest → host.**
- `wasi:keyvalue`: reads are bounded by the memory limit, batches by 1,000 keys,
  keys are base64url file names, and guests can't create symlinks or hard links,
  so no read follows one.
- Calls between objects (`Node::wait_for`): the cycle check and record are under
  the table lock; the caller's concurrency permit is taken after the record and
  released with it; bodies are capped.
- Held publishes: only their bodies are kept (the object is named when they are
  sent), bounded together per request by `max_response_body_kb`, each counting
  its slot (N-692; they used to keep the guest's headers too).
- Guest headers: wasi-http strips `host`, `connection`, `transfer-encoding`,
  `upgrade` and the rest of its forbidden list from guest requests and
  responses, and refuses duplicate `content-length`; a body shorter or longer
  than its declared length ends the connection at every hop.
- Guest logs: one record of at most 4 KiB per write, a 256-record queue, charged
  to the guest's CPU budget.
- Guest file descriptors (one per open file, up to `max_resources`) can exhaust
  only their own app's worker.

**Client → proxy → node.**
- `nanovm-*` headers: names are lower-cased by `http`, every value of each is
  removed, and the proxy's own replace them (`HeaderMap::extend` replaces).
- Hop-by-hop: `Connection`-listed names are removed after the app is resolved,
  so listing `Host` or `Content-Type` changes nothing that matters.
- Smuggling: hyper parses on both sides and re-derives framing; with
  `Content-Length` before a chunked `Transfer-Encoding`, the forwarded request
  drops the length; listing `transfer-encoding` in `Connection` leaves a stated
  length that hyper enforces (it truncates a longer body, aborts a shorter one).
  `CONNECT` reaches no guest (the forwarded target has no object path).
- Routing: the app is keyed by the owner name in the resolver's answer, not the
  request's host, so host spellings (case, port, trailing dot) can't make a new
  app or reach another's; a CNAME to someone else's record serves their app as
  they configured it. The caches are bounded and single-flight.
- WebSocket handshakes: the kept headers exclude content type, connection id,
  framing and hop-by-hop headers; each event's `Content-Type` and
  `Connection-Id` are inserted, replacing any.
- The private publish listener trusts the network, as designed (Q55).

**Registry and Sigstore.**
- `sigstore-verify` 0.15.1 matches the identity exactly
  (`IdentityMatcher::Exact`) and the issuer exactly, and checks the signature
  binds the manifest digest. The tag is resolved to the hash of the manifest
  the proxy fetched; the node fetches by digest and checks every blob's hash
  against the signed manifest. The version floor reads the signed manifest's
  annotation, before any blob is fetched.
- Sizes: 4 MiB metadata, `max_component_mb` components, 16 signatures; four
  fetches at once, each within `load_timeout_ms`. Redirects (at most 5), token
  realms and every resolved address must be public; the client ignores system
  proxies.
- `app_name`: a 64-bit hash of the whole `host registry/repository` string makes
  collisions infeasible; digests are checked before they name a path.

## Docs to fold in

README "The jail" (the `socketpair` bullet), "WebSockets" (how events are
recognised), the proxy list (64 KiB heads) and "Then the worker" (the resource cap),
and PLAN's per-store caps and `max_resources` row, were updated with the fixes.

## Tests run

- Focused, each fix: `jail_blocks_escapes`, `supervised_workers_contain_attacks`,
  `kv_works_in_a_jailed_worker`, `the_proxy_routes_hosts_by_dns`,
  `the_proxy_drops_hop_by_hop_headers`, `egress_*`,
  `jailed_workers_export_otel_without_guest_network_access`, `ws::` and
  `egress::` unit tests.
- Mutation checks: each new assertion was run against the code without its fix
  and failed as shown above, then the fix was restored.
- Full `cargo test --locked -p nanovm` with N-75 in: 109 unit tests pass; 67
  integration tests pass, 10 are ignored (they need the internet or the WASI SDK).
  The one failure, `a_node_runs_at_most_max_apps`, comes from host load. It also
  failed in the run before N-75 (with `cpu_hogs_leave_cores_for_short_requests`),
  Run alone back to back on this host (load ~16), it passed 4 of 11 times with
  N-75 and 3 of 3 without it. Each instrumented failure had app `c`'s cold start
  at 800–900 ms, and the passing run had it at 667 ms. The test needs that to be
  under `b`'s 900 ms sleep minus its 200 ms wait, or `b` is idle when `a` arrives
  and `a` takes its slot. Nothing in N-75 touches cold start, so this reads as
  load, but rerun it on a quiet host to be sure.
- The 16 MiB cap (256 resources) holds for the counter's whole suite: WebSockets,
  key-value paging, calls between objects, the front end. Outbound fetches hold few
  resources at once because `max_outbound_concurrency` (4) limits them. The
  SQLite and JS suites still need a run against it.
