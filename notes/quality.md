# Quality notes (quality agent, round 3)

Test and verification work from IDEAS.md sections D and G. Each item is on its own branch from
`cf/main-next`; this file collects the notes. Commands run from the repository root.

## D-45: fuzzing (`ideas/fuzz`)

**What it is.** A `cargo-fuzz` crate in `fuzz/` (its own workspace, so the node's never builds
libFuzzer) with six targets, each one line calling an entry point in `fuzz/entry.rs`. That file is
compiled into nanovm only with its new `fuzzing` feature (`lib.rs`: `#[cfg(feature = "fuzzing")]
#[path = "../../../fuzz/entry.rs"] pub mod fuzz;`, below the test module, so not in the runtime
count). Visibility changes are `pub(crate)` only: `ws::{parse, messages, Event::encode}`,
`proxy::{parse, unquote, Route}`, `journal::{Undo, encode, decode}`, `telemetry::{stamp,
identity}`. Each target checks properties, not just "no panic":

| Target | Input | Properties |
| --- | --- | --- |
| `ws_events` | an object's WebSocket-over-HTTP answer | parse → encode → parse is the same and no longer; events built from the bytes round-trip |
| `grip_publish` | a guest's GRIP publish | ≤ 64 items; messages no bigger than the publish; where the pre-N-72 `serde_json::Value` parser also accepts it, both give the same messages (differential) |
| `txt_record` | a `_nanovm` TXT record | what the proxy accepts, the node accepts as the same app name, trust pairs and floor (builds the headers `Proxy::resolve` sends and parses them as `AppRef::from_headers`) |
| `app_ref` | the `nanovm-app`/`-trust`/`-min-version`/`-host` headers | a valid sha256 digest; the cached parse equals a fresh one; the app name is one valid path component |
| `journal_log` | an undo log | records start in order and re-encode to their bytes; built records round-trip; every cut of a log (a crash mid-append) decodes to exactly the whole records before it; cancelling a rename keeps the framing |
| `telemetry_stamp` | a worker's OTLP batch | a stamped batch that decodes has only the stamped resource and no identity attribute anywhere; when the input decodes, the output equals decode-replace-scrub (N-74's oracle, for any batch) |

Seeds (`fuzz/seeds.py`, including the crash inputs found) and dictionaries (`fuzz/dicts`).
`.github/workflows/fuzz.yml` runs every target for 5 minutes nightly and on demand, uploading a
crash's input.

**Runs** (ASan, stable rustc with `RUSTC_BOOTSTRAP=1`, 10 minutes each, 4 shared cores):
`ws_events` 1.84 M executions, `grip_publish` 5.5 M, `telemetry_stamp` 2.9 M, `journal_log` 174 k
(it re-decodes 64 cuts per input): no failures, and no disagreement between the typed and the
generic publish parsers.

**Found:**
- **N-507** (`txt_record`, in seconds): the proxy accepted `trust=https://a,b s`; the node splits
  `nanovm-trust` on commas and refused every request for the host. Fixed on `ideas/fix-trust-comma`
  (bd058de, merged into this branch so the target passes): the proxy refuses such a record;
  `accepted_records_are_policies_the_node_accepts` fails without it.
- **N-506** (`app_ref`, after 240 k executions): a `nanovm-host` of `.com` gives an app name starting
  with `.`, which isn't a valid name. Only the proxy sets that header, from a DNS owner name, so it's
  low; open, and the property is scoped around it.

**Not done:** a second round after the fixes. The ASan build of nanovm's dependencies is 4 GB and
was deleted when the shared disk ran short; the stripped binaries (84 MB) ran the rounds above.
Rebuild with `RUSTC_BOOTSTRAP=1 cargo fuzz build --fuzz-dir fuzz` (or nightly), then
`python3 fuzz/seeds.py` and `cargo fuzz run --fuzz-dir fuzz <target> fuzz/corpus/<target> --
-dict=fuzz/dicts/<target>.dict -max_total_time=600`.
