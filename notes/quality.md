# Quality notes (quality agent, round 3)

Test and verification work from IDEAS.md sections D and G. Each item is on its own branch from
`cf/main-next`; this file collects the notes. Commands run from the repository root.

## D-58: hostile guest, second edition (`ideas/hostile2-final`)

**What it is.** A new test-only component, `crates/nanovm/tests/hostile2` (outside `guests/**`),
and `crates/nanovm/tests/hostile2.rs`, nine tests on in-process nodes with the first edition's
limits (32 MiB, 100 ms CPU, 1 s deadline, 8 MiB disk, 2 outbound). Each attack runs beside a
canary object where that matters. Numbers from one run on the shared 4-core host (load ~13):

| Attack | Contained by | Evidence |
| --- | --- | --- |
| runaway recursion | a wasm trap (the guest's shadow stack runs out in linear memory first: `out of bounds memory access`; or `call stack exhausted`) | 500 `trap`; canary's slowest answer under 200 ms; the object restarts |
| pathological regex (`(a+)+$` on `a…a!`, n=40, by backtracking) | the CPU budget | 500 `cpu` well under the 1 s deadline; canary under 200 ms |
| hash flooding (header names solved to share one bucket of `http::HeaderMap`'s FNV hash) | the map's switch to a keyed hash after a long probe, and the 64 KiB per-map limit | 800 colliding names: median 470 µs per map vs 222 µs for ordinary names (2.1x, test bound 3x) |
| huge header counts | the 64 KiB limit both ways | a guest asking for 1,000,000 response headers gets 830; 5,000 request headers get 431 and the server carries on |
| slow uploads (1 byte per 100 ms) | the body is read before the request is queued | the uploading object answers others meanwhile; but nothing bounds how long an upload may take (N-505) |
| outbound connection storm (50 at once) | `max_outbound_concurrency` | 2 answered, 48 refused at once (`ConnectionLimitReached`); upstream peak 2 |
| rename storm (one file back and forth, then fail) | CPU budget / deadline, journal budget | ended after 1.2-2.6 s (`timeout` or `cpu`; fsync waits aren't charged as CPU); rolled back; the next start took 0.27 ms; canary under 10 ms |
| unusual names (NFC/NFD, RTL override, zero-width, emoji, newline, `.`/`..` look-alikes, 255 and 256 bytes) | — | commit and roll back exactly; 256 bytes gives `name-too-long` |
| a directory chain past PATH_MAX, one descriptor at a time | the class fix | refused with `name-too-long` at the depth where the path would reach PATH_MAX; a 12-deep chain commits and rolls back |

**Found (fixed on `ideas/recover-class`, one commit with N-502):**
- **N-503:** `/names?then=fail` (a 256-byte name in a failing request) left the object answering 503
  `instantiate-failed` ("File name too long") for good: `creating` recorded the create because
  `symlink_metadata` failed with ENAMETOOLONG, and recovery treated that as fatal.
- **N-504:** a chain of 16 directories of 255-byte names (path past PATH_MAX under a 72-byte volume
  path) committed with `commit-failed` ("File name too long") and then bricked the object the same
  way; depths 12-15 were fine.

The fix closes the class at once (as the orchestrator asked): `creating` and `removing_dir` record
only for NotFound and return any other error (mapped to the WASI code the syscall gives),
`renaming` refuses moves that would put anything at or past PATH_MAX, and recovery treats
path-shape errors as "nothing there" for every record. Its journal tests include a seeded random
test over these path shapes. This branch carries that commit; the names and deep-path tests fail
without it.

**Open: N-505.** Request bodies have no deadline at any hop (proxy, supervisor, worker): a client
can hold a connection, a link and a task as long as it trickles bytes, within
`max_request_body_kb`. It doesn't hold the object. Per-client limits in the proxy (IDEAS G-75) are
the natural place.

Run: `cargo test --locked -p nanovm --test hostile2 -- --nocapture` (builds the fixture into
`target/test-guest`; ~15 s).
