# Feed aggregator

RSS and Atom feeds fetched once and shared by every reader, in about 150
lines of Rust ([src/lib.rs](src/lib.rs)) on [nanovm-sdk](../../sdk/rust) and
`roxmltree`.

```sh
tools/build-app.sh examples/feeds -o examples/target/feeds.wasm --run --manifest examples/feeds/nanovm.json
```

```sh
curl -XPUT http://127.0.0.1:8080/f-rust/ -d '{"url":"https://blog.rust-lang.org/feed.xml"}'
curl -XPOST http://127.0.0.1:8080/f-rust/refresh            # from cron: {"new":10}
curl -XPUT http://127.0.0.1:8080/ann/follows -d '["f-rust"]'
curl http://127.0.0.1:8080/ann/timeline
```

What it shows:

- **The feed object is the cache.** Each feed (`/f-{name}/`) fetches its
  document over egress when an external clock asks, with `If-None-Match`
  (a `304` costs nothing), parses RSS or Atom, and stores each item once,
  keyed by its guid, with a sequence number. However many readers follow
  it, the feed is fetched once per refresh.
- **Fan-in through calls.** A reader (`/{reader}/`) keeps how far it has
  read each feed; its timeline asks each followed feed for the items after
  that (`object(feed).get("/items?after=…")`) and adds them, one entry per
  link even when two feeds carry the same post.
- **Bad input is reported, not stored.** A feed that answers an error or
  isn't XML gives `502`; nothing it said is kept.
- **Egress is an allowlist.** Only hosts in the manifest's `egress` that a
  node's operator also allows (its `[limits] egress`) can be fetched.

The test runs this app on an in-process dev node with feeds served on
loopback.

API: `PUT /f-{name}/` `{"url"}`; `POST /f-{name}/refresh` → `{"new"}` (with
`"unchanged":true` on a `304`) or `502`; `GET /f-{name}/items?after=N` → up to
100 `[{"seq","feed","title","link"}]`; `PUT /{reader}/follows` `["f-…"]` (up
to 50); `GET /{reader}/timeline` → newest first, at most 500.

Test: `feeds_fetch_once_and_fan_in_to_readers` in
[crates/nanovm/tests/examples.rs](../../crates/nanovm/tests/examples.rs).
