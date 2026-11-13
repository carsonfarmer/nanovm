# LLM gateway

Per-user daily token quotas in front of a streaming completion API, in
about 140 lines of Rust ([src/lib.rs](src/lib.rs)) on
[nanovm-sdk](../../sdk/rust).

```sh
echo '{"upstream_key": "…"}' > ~/gateway-secrets.json
tools/build-app.sh examples/gateway -o examples/target/gateway.wasm --run --manifest examples/gateway/nanovm.json --secrets ~/gateway-secrets.json
```

```sh
curl -N -XPOST http://127.0.0.1:8080/ann/complete -d '{"prompt":"Write a haiku"}'   # data: {"token":…} …
curl http://127.0.0.1:8080/ann/usage                                                # {"used":17,"limit":2000,"day":…}
```

Set `upstream` (a completion API that streams Server-Sent Events) in the
manifest's config and its host in `egress`. Its key is the app's one secret
(`upstream_key`, declared in the manifest's `secrets`): `--secrets` gives it
locally, a node's operator on a node ([Secrets](../../README.md#secrets)).

What it shows:

- **A quota per user, exact.** Each user is an object (`/{user}/`), so
  "check, then spend" can't race, however many requests a user sends at
  once.
- **Streaming through.** `fetch(..).stream()` reads the upstream's answer as
  it is generated and `Response::stream` passes each piece on at once,
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
  `Streaming::next` is an error, not an end) keeps its reservation; an
  upstream error status is answered `502` before anything is reserved.
- **Streams hold their object.** While a user's completion streams, their
  other requests wait; other users are not held up. Long completions need a
  `post_return_timeout_ms` to match.

The API key is never in the package. The tests run this app on an
in-process dev node, and as an operator would (published without the key,
which a supervised node gives its jailed worker), with a mock API on
loopback.

API: `POST /{user}/complete` `{"prompt"}` → `text/event-stream`, `429
{"error":"quota","used","limit"}`, `502` if the upstream fails, `400`;
`GET /{user}/usage` → `{"used","limit","day"}`.

Tests: `gateway_streams_completions_within_quotas`,
`review_gateway_counts_tokens_split_across_http_chunks` and
`oauth_and_gateway_get_their_keys_from_the_node` in
[crates/nanovm/tests/examples.rs](../../crates/nanovm/tests/examples.rs).
