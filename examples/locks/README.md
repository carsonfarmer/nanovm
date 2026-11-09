# Locks and leases

A lock service with expiring leases and fencing tokens, in about 60 lines of
Rust ([src/lib.rs](src/lib.rs)) on [nanovm-sdk](../../sdk/rust).

```sh
tools/build-app.sh examples/locks -o examples/target/locks.wasm --run --manifest examples/locks/nanovm.json
```

```sh
curl -XPOST 'http://127.0.0.1:8080/reindex/acquire?owner=worker-1&ttl_ms=30000'
#   {"owner":"worker-1","token":1,"expires_at":1762617600000}
curl -XPOST 'http://127.0.0.1:8080/reindex/acquire?owner=worker-2'   # 409, with worker-1's lease
curl -XPOST 'http://127.0.0.1:8080/reindex/renew?owner=worker-1&token=1&ttl_ms=30000'
curl -XPOST 'http://127.0.0.1:8080/reindex/release?owner=worker-1&token=1'   # 204
```

What it shows:

- **Single-writer semantics, for free.** Each lock is an object
  (`/{lock}/`), and an object serves one request at a time, so ten
  concurrent acquires have exactly one winner, with no compare-and-swap.
  The runtime guarantees one live instance per object id.
- **Leases end by themselves.** A lease records when it expires and the
  next request sees it as free after that; nothing has to run at expiry.
- **Fencing tokens.** Each new grant's token is larger than every earlier
  one (a counter in the object's store, kept across releases, expiries and
  restarts). A holder that paused past its lease (a GC pause, a partition)
  is refused when it tries to renew, and the resource it guards should
  refuse writes carrying a token smaller than the largest it has seen.

API: `POST /{lock}/acquire?owner=…&ttl_ms=10000` → `200` lease, or `409`
with the holder's lease (its holder acquiring again renews and keeps the
token); `POST /{lock}/renew?owner=…&token=…&ttl_ms=…` → `200` or `409`;
`POST /{lock}/release?owner=…&token=…` → `204` or `409`; `GET /{lock}/` →
the lease or `null`. A lease is `{"owner","token","expires_at"}` (ms since
the epoch). `owner` is 1-64 bytes, `ttl_ms` 1 ms to 1 hour.

Test: `locks_have_one_holder_and_growing_fencing_tokens` in
[crates/nanovm/tests/examples.rs](../../crates/nanovm/tests/examples.rs).
