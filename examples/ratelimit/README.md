# Rate limiter

A token bucket per key, as a service, in about 70 lines of Rust
([src/lib.rs](src/lib.rs)) on [nanovm-sdk](../../sdk/rust).

```sh
tools/build-app.sh examples/ratelimit -o examples/target/ratelimit.wasm --run --manifest examples/ratelimit/nanovm.json
```

```sh
curl -XPOST http://127.0.0.1:8080/user-42/take          # {"allowed":true,"remaining":9}
curl -XPOST 'http://127.0.0.1:8080/user-42/take?n=20'   # 400: more than the bucket holds
curl -XPUT http://127.0.0.1:8080/login-1.2.3.4/ -d '{"capacity":5,"per_second":0.1}'
curl http://127.0.0.1:8080/user-42/                     # {"tokens":9.4,"capacity":10,"per_second":1}
```

What it shows:

- **Serial objects make it trivial.** Each key is an object (`/{key}/`),
  and an object serves one request at a time, so "refill, check, take" is
  plain code: no lock, compare-and-swap or Lua script. Twenty concurrent
  takes against five tokens give exactly five `200`s.
- **Limits that survive restarts.** The bucket is in the object's key-value
  store and commits with each `200`, so a node restart doesn't hand every
  client a fresh bucket, as an in-memory limiter would.
- **Nothing runs between requests.** A bucket refills by the clock, worked
  out when it is next asked; a refused take writes nothing.
- **Config defaults.** `capacity` and `per_second` come from the manifest's
  `config` (`wasi:config`); `PUT /{key}/` sets one key's own.

API: `POST /{key}/take?n=1` → `200 {"allowed":true,"remaining"}` with
`ratelimit-remaining`, or `429 {"allowed":false,"remaining","retry_after_ms"}`
with `retry-after` (seconds); `GET /{key}/` → `{"tokens","capacity","per_second"}`;
`PUT /{key}/` `{"capacity","per_second"}` → `204` (the bucket starts full).

Test: `ratelimit_buckets_are_exact_and_durable` in
[crates/nanovm/tests/examples.rs](../../crates/nanovm/tests/examples.rs).
