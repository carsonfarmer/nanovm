# Wiki with history

A wiki that keeps every revision, refuses lost updates and diffs any two
revisions, in about 170 lines of Rust ([src/lib.rs](src/lib.rs)) on
[nanovm-sdk](../../sdk/rust), `pulldown-cmark` and `similar`.

```sh
export WASI_SDK_PATH=/path/to/wasi-sdk-34.0-x86_64-linux   # SQLite is C
tools/build-app.sh examples/wiki -o examples/target/wiki.wasm --run --manifest examples/wiki/nanovm.json
```

```sh
curl -XPUT 'http://127.0.0.1:8080/team/pages/Home?author=ann' -H 'if-match: 0' -d '# Home'   # {"rev":1}
curl http://127.0.0.1:8080/team/pages/Home                     # <h1>Home</h1>, etag "1"
curl -XPUT 'http://127.0.0.1:8080/team/pages/Home?author=bob' -H 'if-match: 1' -d '# Home, v2'
curl 'http://127.0.0.1:8080/team/pages/Home/diff?from=1&to=2'
```

What it shows:

- **History in SQLite.** Each wiki (`/{wiki}/`) is an object with its own
  database; a save inserts a revision and never overwrites, so every
  version can be read (`?rev=`), listed (`/history`) and diffed (`/diff`).
- **No lost updates, without locks.** A save names the revision it was
  edited from (`if-match`); if the page has moved on, it is refused with
  `409` and the latest revision, for the editor to merge. The check and the
  insert are one statement, and the wiki serves one request at a time, so
  two editors of the same revision can't both win.
- **Libraries work.** `pulldown-cmark` renders Markdown and `similar` makes
  unified diffs, both plain crates built for `wasm32-wasip2`.
- **A page can't run script in its readers' browsers.** Raw HTML in it is
  escaped, and Markdown's own links and images keep only http, https,
  mailto and relative destinations, read as a browser reads them (any
  case; tabs and newlines inside and controls around ignored): a
  `[click](javascript:…)` is shown as its text. The rendered page's
  `Content-Security-Policy` (`default-src 'none'`, images only) refuses
  script besides.

API: `PUT /{wiki}/pages/{name}?author=…` (a name is 1-100 letters, digits,
`_` or `-`; `if-match: <rev>`, `0` for a new page; body: Markdown) → `201
{"page","rev"}`, `409 {"error","rev"}` or `428` without `if-match`; `GET
/{wiki}/pages/{name}` (`?rev=n`, `?raw=1`) with `etag`; `GET
/{wiki}/pages/{name}/history` → `[{"rev","author","at","bytes"}]`; `GET
/{wiki}/pages/{name}/diff?from=&to=`; `GET /{wiki}/` → `[{"page","rev"}]`.

Tests: `wiki_keeps_every_revision_and_refuses_lost_updates` and
`review_wiki_does_not_serve_executable_markdown_links` in
[crates/nanovm/tests/examples.rs](../../crates/nanovm/tests/examples.rs).
