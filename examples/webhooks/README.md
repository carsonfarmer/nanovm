# Webhook receiver and relay

Signed webhooks in, relayed out with retries, in about 150 lines of Rust
([src/lib.rs](src/lib.rs)) on [nanovm-sdk](../../sdk/rust), `hmac` and
`sha2`.

```sh
tools/build-app.sh examples/webhooks -o examples/target/webhooks.wasm --run --manifest examples/webhooks/nanovm.json
```

```sh
curl -XPUT http://127.0.0.1:8080/github/config -d '{"secret":"s3cret","targets":["https://hooks.example.com/ci"]}'
# then point a GitHub webhook (secret s3cret) at http(s)://<your host>/github/in
curl -XPOST http://127.0.0.1:8080/github/retry   # from cron, every minute
curl http://127.0.0.1:8080/github/deliveries
```

What it shows:

- **Verify, store, answer.** Each endpoint (`/{hook}/`) keeps its secret and
  targets in its own store. A webhook's `x-hub-signature-256` is checked
  (HMAC-SHA256, compared in constant time), the delivery is stored, and the
  sender gets `202` straight away.
- **Work after the answer.** The relay runs in `nanovm_sdk::after`: once the
  answer has gone, before the object takes another request, within
  `post_return_timeout_ms`; what it records commits when it is done. The
  sender never waits for the targets (in the test, `202` comes back while a
  target takes 1.5 s to answer), but the next request to the same endpoint
  does.
- **Egress is an allowlist.** Targets are reached with `fetch`, and only
  hosts in the manifest's `egress` list (that a node's operator also allows
  in its `[limits] egress`) can be reached: a target outside it
  fails with `HttpRequestDenied`, recorded like any other failure. Private
  addresses are refused even if listed (except on a dev node), so an app
  can't be used to reach the node's network.
- **Retries without timers.** A failed target is retried with backoff (1, 2,
  4, 8 s) when an external clock sends `POST /{hook}/retry`; after five
  rounds the delivery is dead. Each attempt carries `x-relay-delivery`, so a
  target can drop repeats.

Anyone can change an endpoint's config here; a real relay puts it behind
sign-in. The test runs this app on an in-process dev node, whose egress may
reach targets on loopback; `nanovm run`, like a production node, refuses
private addresses.

API: `PUT /{hook}/config` `{"secret","targets":[up to 10 URLs]}` → `204`;
`POST /{hook}/in` (signed) → `202 {"id"}`, `401` if the signature is wrong,
`404` before config; `POST /{hook}/retry` → `{"retried"}`; `GET
/{hook}/deliveries` → `[{"id","body","pending","done","tries","next_at","dead","error"}]`.

Test: `webhooks_verify_answer_then_relay_with_retries` in
[crates/nanovm/tests/examples.rs](../../crates/nanovm/tests/examples.rs).
