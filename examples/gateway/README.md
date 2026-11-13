# LLM gateway

Per-user daily token quotas in front of a streaming completion API, in
about 140 lines of Rust ([src/lib.rs](src/lib.rs)) on
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
- **Streaming through.** `fetch_streaming(..)` reads the upstream's answer as
  it is generated and `Body::stream` passes each piece on at once,
  counting tokens on the way. The count parses the Server-Sent Events
  across reads (an event ends at a blank line, wherever the network split
  it; LF, CRLF or CR line ends), not substrings of each read: an event cut
  in two by the reads is still one token.
- **Charge first, refund after.** A stream runs after the handler has
  answered, and what it changes commits only when it ends; if it is cut off
  by `post_return_timeout_ms`, its changes roll back. Counting tokens only
  at the end would make a cut-off stream free. So the request reserves
  `max_tokens` before answering (that commits with the answer), and the
  stream gives back what it didn't use when it verifiably finishes: the
  upstream said `[DONE]` and its body arrived whole. What it used is the
  upstream's own count when it reports one (`usage.completion_tokens`, as
  OpenAI's API does), else the token events seen. A stream that ends
  without `[DONE]` keeps its reservation. A stream cut off
  by the client, the deadline or the upstream (its body breaks off:
  `Streaming::next` is an error, not an end) keeps its reservation. An
  upstream that breaks off breaks the client's stream off too (the stream
  returns that error), so the client can tell a cut answer from a whole
  one. An upstream error status is answered `502` before anything is
  reserved.
- **Streams hold their object.** While a user's completion streams, their
  other requests wait; other users are not held up. Long completions need a
  `post_return_timeout_ms` to match.

The API key is in the manifest's `env`, which is published with the app;
real secrets need a secret store (IDEAS G-78). The test runs this app on an
in-process dev node with a mock API on loopback.

API: `POST /{user}/complete` `{"prompt"}` → `text/event-stream`, `429
{"error":"quota","used","limit"}`, `502` if the upstream fails, `400`;
`GET /{user}/usage` → `{"used","limit","day"}`.

Tests: `gateway_streams_completions_within_quotas` and
`review_gateway_counts_tokens_split_across_http_chunks` in
[crates/nanovm/tests/examples.rs](../../crates/nanovm/tests/examples.rs).
