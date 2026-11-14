# Notes with full-text search

Notes with ranked, highlighted search over an FTS5 index, in about 90 lines
of Rust ([src/lib.rs](src/lib.rs)) on [nanovm-sdk](../../sdk/rust).

```sh
export WASI_SDK_PATH=/path/to/wasi-sdk-34.0-x86_64-linux   # SQLite is C
tools/build-app.sh examples/notes -o examples/target/notes.wasm --run --manifest examples/notes/nanovm.json
```

```sh
curl -XPOST http://127.0.0.1:8080/ann/notes -d '{"title":"Soup","body":"Leek and potato soup."}'
curl 'http://127.0.0.1:8080/ann/search?q=pota'   # [{"id":1,"title":"Soup","snippet":"Leek and [potato] soup."}]
```

What it shows:

- **A search index per person.** Each person's notes are an object
  (`/{user}/`) whose SQLite database is an FTS5 table, so the index is as
  private and as durable as the notes. No search cluster to run, feed or
  keep in step: saving, editing or deleting a note updates the index in
  the same statement and the same commit.
- **Ranked, highlighted, forgiving.** Results are ranked by BM25 with the
  title weighted five times the body; `snippet()` marks the matches; the
  Porter stemmer matches "simmering" to "simmer"; each word is a prefix.
- **User input is never query syntax.** Each word of `q` is quoted, so
  `"NEAR(` searches for words, not FTS5 operators, and can't fail the query.

Making this work found N-603: the SQLite provider refused the read-only
pragma FTS5 asks for; it now allows it.

API: `POST /{user}/notes` `{"title","body"}` (up to 200 bytes and 64 KiB) →
`201 {"id"}`; `GET`/`PUT`/`DELETE /{user}/notes/{id}`; `GET
/{user}/search?q=…` (1-10 words) → the best 20 `[{"id","title","snippet"}]`.

Test: `notes_search_their_own_full_text_index` in
[crates/nanovm/tests/examples.rs](../../crates/nanovm/tests/examples.rs).
