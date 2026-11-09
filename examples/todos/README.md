# Offline-first todo sync

Devices that work offline and sync through one object per person, with
field-level merges and reported conflicts, in about 120 lines of Rust
([src/lib.rs](src/lib.rs)) on [nanovm-sdk](../../sdk/rust).

```sh
tools/build-app.sh examples/todos -o examples/target/todos.wasm --run --manifest examples/todos/nanovm.json
```

```sh
curl -XPOST http://127.0.0.1:8080/ann/sync -d '{"since":0,"ops":[{"id":"op1","todo":"t1","base":0,"title":"milk"}]}'
# {"version":1,"changes":[{"id":"t1","title":"milk",…}],"conflicts":[]}
```

What it shows:

- **The object is the sync point.** Each person's list is an object
  (`/{user}/`), so their devices' syncs are applied one at a time, in the
  order they arrive, with no lock and no lost write.
- **A protocol a client can queue.** A device records what it changes as
  operations, each with an id and the version of the todo it last saw
  (`base`), and sends them all in one `POST /sync` with the list version it
  last had (`since`). The answer is every todo changed since then; the device
  replaces its copy with them.
- **Merges and conflicts.** Each field remembers the version that last wrote
  it. An operation that writes a field nobody changed since its `base`
  applies, so edits to different fields (one device renames, the other ticks
  done) merge. A field changed since by another device is a conflict: the
  first to sync wins, and the later device is told which fields of which
  todo, to show its person. A delete wins over a later edit based on before
  it.
- **Retries are safe.** An operation id is applied once; resending a sync
  whose answer was lost changes nothing (its conflicts were reported the
  first time, and its changes come back again).

Applied operation ids are kept as keys, one file each, so `max_files` bounds
how many a list can remember; a real service would forget ids older than any
device's last sync.

API: `POST /{user}/sync` `{"since","ops":[{"id","todo","base","title"?,"done"?,"delete"?}]}`
(up to 100 ops; ids 1-64 of `A-Za-z0-9_-`) → `{"version","changes":[{"id","title","done","deleted","version","written"}],"conflicts":[{"op","todo","fields"}]}`.

Test: `todos_sync_offline_edits_and_report_conflicts` in
[crates/nanovm/tests/examples.rs](../../crates/nanovm/tests/examples.rs).
