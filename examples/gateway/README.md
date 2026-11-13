# LLM gateway

Per-user daily token quotas in front of a streaming completion API, in
about 70 lines of Rust ([src/lib.rs](src/lib.rs)) on
[nanovm-sdk](../../sdk/rust).

```sh
tools/build-app.sh examples/gateway -o examples/target/gateway.wasm --run --manifest examples/gateway/nanovm.json
```

```sh
curl -N -XPOST http://127.0.0.1:8080/ann/complete -d '{"prompt":"Write a haiku"}'   # data: {"token":…} …
curl http://127.0.0.1:8080/ann/usage                                                # {"used":17,"limit":2000,"day":…}
```

Set `upstream` (a completion API that streams Server-Sent Events) in the
manifest's config, its host in `egress`, and its key as `UPSTREAM_KEY` in
`env`.

What it shows:

- **A quota per user, exact.** Each user is an object (`/{user}/`), so
  "check, then spend" can't race, however many requests a user sends at
  once.
- **Streaming through.** `fetch(..).stream()` reads the upstream's answer as
  it is generated and `Response::stream` passes each piece on at once,
  counting tokens on the way.
- **Charge first, refund after.** A stream runs after the handler has
  answered, and what it changes commits only when it ends; if it is cut off
  by `post_return_timeout_ms`, its changes roll back. Counting tokens only
  at the end would make a cut-off stream free. So the request reserves
  `max_tokens` before answering (that commits with the answer), and the
  stream gives back what it didn't use when it finishes. A stream cut off
  by the client, the deadline or the upstream (its body breaks off:
  `Streaming::next` is an error, not an end) keeps its reservation; an
  upstream error status is answered `502` before anything is reserved.
- **Streams hold their object.** While a user's completion streams, their
  other requests wait; other users are not held up. Long completions need a
  `post_return_timeout_ms` to match.

The API key is in the manifest's `env`, which is published with the app;
real secrets need a secret store (IDEAS G-78). The test runs this app on an
in-process dev node with a mock API on loopback.

API: `POST /{user}/complete` `{"prompt"}` → `text/event-stream`, `429
{"error":"quota","used","limit"}`, `502` if the upstream fails, `400`;
`GET /{user}/usage` → `{"used","limit","day"}`.

Test: `gateway_streams_completions_within_quotas` in
[crates/nanovm/tests/examples.rs](../../crates/nanovm/tests/examples.rs).
