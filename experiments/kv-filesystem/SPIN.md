# Spin async KV over the transactional filesystem

Prototype only; native KV, config/env injection and `guests/**` remain unchanged.
No package is published. The provider uses filesystem imports only, without a
private host API, cache, database or second commit mechanism.

## Reproduce

Install the prerequisites in [README](README.md), then:

```sh
bash experiments/kv-filesystem/build.sh
ulimit -n 100000
env -u RUST_BACKTRACE cargo run --locked -p nanovm --example spin_kv_probe
```

CI runs both this positive probe and the earlier synchronous negative control.
The native runner checks exact results from an actual composed component on Node.

## Interface and storage

`spin-wit/deps/spin-key-value/key-value.wit` is copied **unchanged** from
[Spin v4.2.6](https://github.com/spinframework/spin/blob/v4.2.6/wit/deps/spin-key-value%403.0.0/key-value.wit),
under its Apache-2.0 license (included beside it). This is Spin's existing
`spin:key-value@3.0.0`, not an invented async WASI interface. No batch/atomics API.

Store labels use native KV's directory namespace; keys retain its base64url file
layout. The prototype accepts valid labels rather than implementing Spin's
manifest allowlist: deployment policy integration is required before adoption.
It rejects empty or >180-byte keys and values >1 MiB, and caps stores plus live
listings at 32 per provider instance. Those are explicit prototype policies, not
claims about the Spin standard or replacements for host quotas.

Set creates an exclusive temporary file, writes it, then renames over the key.
Concurrent provider writes have different temporary names. Failure removes the
temporary; cleanup failure traps to require rollback. Replacement preserves old
open descriptors. This temporarily needs space for both values and an extra file,
so near-quota behavior differs from native KV's reservation scheme.

Get reads bounded chunks, checking the value limit while reading. Listing reads
one filesystem entry, decodes one key, then awaits the output stream before
reading another. It does not collect keys into a list. It ignores non-files and
invalid key filenames. The filesystem and listing completion futures propagate
errors; stream closure alone is not success. Dropping the output stream stops
enumeration. A listing owns its directory independently of its Store handle.

## Executed checks

- CRUD, missing keys, empty values, invalid labels and
  keys, oversized-value rejection, UTF-8/path-like key encoding.
- Direct file → Spin KV → direct file and native KV ↔ Spin KV visibility.
- Old open descriptor still reads the old bytes after replacement.
- Stream all 200 keys, checking exact contents; empty listing; removed directory
  reports an error through the completion future.
- 100 partial-read cancellations and 100 unread cancellations; capacity returns.
- 30 simultaneously backpressured listings (plus two stores), the next listing
  reports resource exhaustion, then slots recover after cancellation.
- Concurrent differently sized writes to one key produce one complete value.
- Committed value survives trap rollback and a Node restart.
- Host disk quota refuses writes; a refused replacement preserves the old value;
  no temporary files remain after these operations.

Full locked suite: 107 unit and 67 integration passed, ten integration ignored
(optional SQLite fixtures and external HTTPS). Experiment release clippy passes.
These checks do not claim optional SQLite tests ran locally.

Mutation check: replacing listing completion errors with `Ok(())` makes the
resource-pressure probe fail. Restored the provider with `git checkout --`,
confirmed the entire diff was empty, rebuilt and reran the passing probe.

Build size: provider 173,592 bytes (about 170 KiB). Production
host runtime LOC is 5,336 → 5,336: this is an additional experimental component, not
yet a host-code reduction. No latency, cold-start, memory-density or throughput
benchmark was run; no demo claim.

## Before adoption

Still test cancellation of in-flight mutations, abandonment of both listing
handles, stalled streams across post-return deadlines, concurrent direct-file
mutation, mixed SQLite transactions, crash cuts, object isolation and file-count
quota boundaries. No snapshot or ordering guarantee is added to listing.
Aborting a write between temporary creation and cleanup needs particular review;
do not treat this prototype as a production-safe provider.

The next step is that focused lifecycle/safety review and a matched native-KV
performance comparison, not removal of native KV. Existing guests would need an
explicit migration decision later.

**Successor prompt:** Continue `spin-kv-prototype`; read Q74 and this file. Spin's
async composition works, and streaming/backpressure/cancellation probes pass.
Keep the upstream WIT unchanged, env/config host-injected and guests untouched.
Close the lifecycle and policy gaps above and measure costs before recommending
replacement. Do not confuse this successful experiment with production adoption.
