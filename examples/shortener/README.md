# URL shortener

Short links with click counts per day, in about 100 lines of Rust
([src/lib.rs](src/lib.rs)) on [nanovm-sdk](../../sdk/rust).

```sh
tools/build-app.sh examples/shortener -o examples/target/shortener.wasm --run --manifest examples/shortener/nanovm.json
```

```sh
curl -XPOST http://127.0.0.1:8080/new/ -d https://example.com   # {"code":"k3x9pqa","url":"https://example.com"}
curl -i http://127.0.0.1:8080/k3x9pqa                           # 302, location: https://example.com
curl http://127.0.0.1:8080/k3x9pqa/stats                        # {"url":…,"clicks":1,"days":{"2026-11-08":1}}
curl -XPOST http://127.0.0.1:8080/docs/ -d https://example.com/docs   # a chosen code; 409 if taken
```

What it shows:

- **Sharding is the URL.** Every link is its own object (`/{code}`), with its
  target and counters in its own key-value store. Links share nothing, so a
  hot link never slows another, and there is no table of all links to grow
  or lock.
- **Create-if-absent needs no lock.** An object serves one request at a time,
  so "is this code free? then take it" can't race: two `POST /docs/` can't
  both win.
- **Calls between objects.** `POST /new/` is served by the object `new`,
  which picks a random code and asks that object to take the link
  (`object(code).post("/", url)`), retrying if the code is taken.
- **Every redirect is a durable write.** The click counters commit with the
  `302`; if counting failed, the redirect would not be sent.

API: `POST /new/` (body: the URL) → `201 {"code","url"}`; `POST /{code}/` →
`201` or `409`; `GET /{code}` → `302`; `HEAD /{code}` → `302` without
counting; `GET /{code}/stats` → `{"url","clicks","days":{"YYYY-MM-DD":n}}`.

Test: `shortener_redirects_and_counts_clicks` in
[crates/nanovm/tests/examples.rs](../../crates/nanovm/tests/examples.rs).
