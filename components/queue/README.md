# Queue: an append-only log with consumer positions

A Wasm component that gives an app durable topics (append-only logs) with
named consumer positions, stored in the object's own `/data` volume. The host
knows nothing about it: compose it into the app with `wac plug`, like the
[KV](../keyvalue/README.md) and [SQLite](../sqlite/README.md) providers.

```sh
bash components/queue/build.sh      # target/components/queue/{provider,tests}.wasm
wac plug app.wasm --plug target/components/queue/provider.wasm -o app-with-queue.wasm
```

The interface is [`nanovm:queue/log@0.1.0`](wit/queue.wit): `topic.open`,
`append` (a batch, all or none), `read(offset, max)`, `bounds`, and per
consumer `position`, `seek`, `next` (read and advance), `consumers`,
`remove-consumer`, plus `trim(before)` for retention. No standard WIT fits:
`wasi:messaging` is a broker client with pushed delivery, not a stored log
read by offset.

## Why it needs no write-ahead log

nanovm commits all of a request's changes to `/data` together, or rolls them
all back (guest error, trap, CPU/time limit, crash). So `next` moving a
consumer's position is atomic with whatever else the request does: if the app
fails while handling the messages, the position rolls back and the same
messages come again. Within the object, that makes processing exactly-once
for its own state. Effects outside it (egress, calls to other objects) still
happen at least once.

Each call is all-or-nothing within the request: an error return means nothing
changed (a refused append truncates what it wrote; a refused new file is
removed). If that undo itself fails, the component traps, which rolls back the
whole request. Never return an HTTP error expecting rollback; return a guest
error (or trap) instead, as the SQLite README explains.

## Storage

`/data/.queue/<topic>/`:

- `<base>.dat`: message payloads back to back; `<base>.idx`: 16 bytes per
  message (end position in `.dat`, append time in ms). `<base>` is the first
  offset, zero-padded to 20 digits. A segment takes no more appends once it
  holds 1 MiB or 16,384 messages; a batch never splits.
- `head`: the oldest retained offset, once trimmed. `c.<consumer>`: a
  consumer's next offset (8 bytes, overwritten in place).

Appends only extend files, so the host's undo journal records each file's old
size once per request rather than copying data. A message costs its payload
plus 16 bytes. Reading one message reads 16-32 bytes of index and the payload.
Topic state (segment list, end of log) is cached in the instance across
requests; a failed request ends the instance, so the cache never outlives a
rollback.

## Limits

- Topic and consumer names: 1-64 ASCII letters, digits, `_` or `-`.
- A message is at most 1 MiB; an append is 1-4,096 messages, at most 4 MiB.
- A read returns at most 1,024 messages and about 1 MiB of payload (always at
  least one message).
- `trim` frees space a segment at a time; `oldest` moves at once.
- At most 64 handles open at once (`table-full`).
- Space: payload + 16 B per message, plus 2 files per segment and 1 per
  consumer against `max_files`. A full quota returns `full` and changes
  nothing.
- Cooperative ownership: don't write `/data/.queue` directly. Objects never
  share topics; another object reaches a queue by calling its object.
- No blocking wait for new messages: a request can't wait for another request
  on the same object (they run one at a time). Poll, or push with WebSockets.

## Cost

Measured with `cargo run --locked -p nanovm --example components_bench --
queue` (in-process node, dev profile with optimized dependencies, serial
requests, this 4-core VM under other agents' load). See
[notes/components.md](../../notes/components.md) for medians and method.

## Tests

`crates/nanovm/tests/components.rs`, `queue_*` (ignored by default; CI runs
them with `--ignored` after `scripts/build-components.sh`): contracts,
persistence across eviction and restart, rollback of a failed request (appends
and positions), segment roll and trim, and disk/file quota exhaustion.
