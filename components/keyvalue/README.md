# Filesystem-backed Spin KV

Branch migration in progress; not yet a production release. The provider exports
unchanged `spin:key-value/key-value@3.0.0` from Spin v4.2.6. The vendored WIT keeps
its upstream Apache license. Compose it into an app with `wac plug`; the host
links only the filesystem, not KV. Env and config remain host-injected.

```sh
bash components/keyvalue/build.sh --test-hooks
cargo run --locked -p nanovm --example spin_kv_probe -- --test-hooks
bash scripts/build-guests.sh
```

`provider.wasm` is always the normal build; fault hooks only enter
`tests-faults.wasm`. The output is under `target/keyvalue`. No build publishes
packages. `scripts/build-guests.sh` composes counter and hostile; SQLite build
scripts compose both providers. Old synchronous KV imports are no longer linked.

## Storage contract

- A store is `/data/.kv/<label>`; valid labels are 1–64 ASCII letters, digits,
  underscores or hyphens. All such labels are allowed within an object's own
  volume. This differs from Spin's deployment-manifest label allowlist; nanovm
  does not interpret `spin.toml` or offer cross-object/shared stores.
- Keys are 1–180 UTF-8 bytes, encoded as unpadded base64url filenames. Values are
  at most 1 MiB. Empty values work. Missing reads return `none`; missing deletes
  succeed. No atomics or batch extension is supplied.
- Set writes an exclusive temporary file, closes it, and renames it over the key.
  Existing descriptors continue to see the old file. KV and direct file access
  see each other's successful changes immediately; neither maintains a cache.
- Replacement needs space for the old and new values and one temporary entry.
  The host separately bounds journal logs and retained files. Catching a failed
  set preserves the old key and removes the temporary file; failed cleanup traps
  to force request rollback. Existing temporary-name collisions are skipped,
  at most 33 candidates, without modifying a pre-existing file.
- Mutations defer caller cancellation until publication or cleanup completes.
  Whole-request traps, deadlines and crashes still use host rollback. The
  provider never commits: the host's two existing commit boundaries apply to
  SQL, KV and ordinary files together.
- Listings stream one key at a time with a separate error completion. At most
  32 combined stores/listings are active; a listing retains its slot until its
  completion is consumed or dropped. Drop both endpoints to abandon a listing.
  Endpoint-close processing is asynchronous. Host resource limits additionally
  bound outstanding endpoints, including rejected listings.

## Validation checkpoint

Executed regressions failed before their fixes for unread completion-slot leaks,
pre-existing temporary-name collisions, and cancellation after temporary-file
creation. All three pass after fixes, along with CRUD, raw-file/descriptor
visibility, streamed listing/backpressure, concurrent replacement, quota cleanup,
trap rollback and restart. The cancellation fixture observes a filesystem marker
inside a started write before dropping its future; it is not an unpolled-call test.

Still required: migrated full suite, SQL/crash integration, filesystem scale and
journal measurements, matched performance, mutation checks, demo and final review.
No production-readiness or total LOC saving is claimed at this checkpoint.

Successor prompt: continue `spin-kv-prototype`, read HANDOFF Q75 and W-18. Preserve
filesystem-only persistence, Spin WIT and injected env/config. Finish the gates
above and count provider/build code as well as removed host code. Branch pushes
do not publish; merging guest changes into main does and needs owner approval.
