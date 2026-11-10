# Docs: JSON document collections with secondary indexes

A Wasm component that stores JSON documents in named collections with
secondary indexes (equality, ranges, unique constraints, multi-value array
fields), in the object's own `/data` volume. The host knows nothing about it:
compose it into the app with `wac plug`, like the [KV](../keyvalue/README.md)
and [SQLite](../sqlite/README.md) providers.

```sh
bash components/docs/build.sh      # target/components/docs/{provider,tests}.wasm
wac plug app.wasm --plug target/components/docs/provider.wasm -o app-with-docs.wasm
```

The interface is [`nanovm:docs/store@0.1.0`](wit/docs.wit): `collection.open`,
`get`, `put` (insert or replace), `delete`, `scan` (id order), `query` (an
index range, paged by cursor), `create-index` / `drop-index` / `indexes`,
`create-text-index` and `search` (full-text, BM25), `stats` and `compact`. No
standard WIT exists for document stores.

**Full-text search (J-91)** is a kind of index here rather than a separate
component: `create-text-index("body")` indexes the words of a string field (or
an array of strings), lowercased runs of letters and digits with no stemming
or stop words, and `search("body", "rust filesystem", 10)` returns documents
containing any of the words, best first by BM25. The postings live in memory
and are rebuilt from the log with the other indexes, so they cost memory per
distinct word per document and add to the cold start.

**If the app already composes SQLite, use it instead**: a table of
`(id TEXT PRIMARY KEY, body TEXT)` with indexes on `json_extract(body, ...)`
gives the same thing plus joins and transactions, at SQLite's size and cost.
This component is for apps that want indexed JSON without a 1 MB C library.

## Design

Bitcask-style. `/data/.docs/<name>/log` is an append-only sequence of put and
delete records; `indexes` lists the index definitions. Every document's
location and its indexed values live in memory (sorted maps), rebuilt from the
log when an instance first opens the collection and then cached for the
instance's life. A change appends one record, so the host's undo journal
records only the log's old size (no data copy). Reads are one positioned read
of the log.

When more than half of a log over 1 MiB is garbage (replaced or deleted
versions), the change that crossed the line rewrites it. The rewrite unlinks
the old log first and reads it through an open descriptor, so it needs no free
space: the host keeps the old file, uncounted, until commit. A full volume can
still delete documents: when even a delete record doesn't fit, the document's
own record is turned into a delete record in place (one byte).

## Why it needs no write-ahead log

nanovm commits all of a request's changes to `/data` together, or rolls them
all back. Each call is all-or-nothing within the request: validation and
unique checks come before the single append, and a refused append is truncated
away, so an error return means nothing changed. If an undo itself fails, or a
rewrite fails after unlinking the old log, the component traps and the whole
request rolls back. Memory is updated only after the file change succeeds, and
a failed request ends the instance, so the cache never outlives a rollback.

## Limits

- Collection names: 1-64 ASCII letters, digits, `_` or `-`. Ids: 1-256 bytes
  of any UTF-8. Bodies: JSON objects up to 1 MiB, stored as given.
- Up to 16 indexes per collection; fields are dotted paths up to 128 bytes.
  Index order: null < false < true < numbers < strings. Numbers compare as
  f64 (integers beyond 2^53 lose precision). Each scalar of an array is indexed
  separately; objects and missing fields are not indexed.
- Pages: 1-1,000 documents, stopping after about 1 MiB of bodies.
- Memory: every id and indexed value of every opened collection, about 100
  bytes per document plus the values. Memory counts against `memory_mb`.
- **Cold start**: the first call after an instance starts reads the whole log
  and parses every document if there are indexes. Measured below at about
  2.6 ms per 1,000 documents with one index, charged to that request's CPU
  budget: at the default `cpu_ms` of 200, keep a collection under about
  50,000 documents, or raise the budget.
- Space: a record costs 9 bytes plus id and body. Garbage up to the size of
  the live data before a rewrite.
- Cooperative ownership: don't write `/data/.docs` directly.

## Cost

See [notes/components.md](../../notes/components.md) (method, medians).

## Tests

`crates/nanovm/tests/components.rs`, `docs_*`: contracts (order across types,
nested paths, arrays, cursors, errors), indexes and unique constraints across
eviction and restart, rollback of failed requests (memory included), log
compaction, quota exhaustion (refused puts, deletes and compaction on a
completely full volume), and full-text search (ranking, case, arrays,
updates, deletes, restart).
