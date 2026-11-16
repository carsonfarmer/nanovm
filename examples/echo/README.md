# Echo

Answers a request with what the app saw of it: a way to see what reaches a
nanovm app and what leaves it, and the test of the SDK's mapping between
`wasi:http` and the `http` crate's types.

```sh
tools/build-app.sh examples/echo -o examples/target/echo.wasm --run
curl -i 'http://127.0.0.1:8080/a/x?status=418' -H 'x-echo-a: 1' -H 'x-echo-a: 2' -d 'hi'
```

- **What the app sees.** `x-seen` is the method and URI as the handler got
  them: the path within the object (`/a/x` arrives as `/x`), the query as
  sent.
- **What leaves it.** The answer has the status asked for (`?status=N`), the
  request's body and `content-type`, and every `x-echo-*` header as it came:
  each value of a repeated header, in order, and bytes that aren't UTF-8.
- **Through a call between objects.** `/{object}/relay/{other}/…` sends the
  same request to object `{other}` as `/…` (`object(other).fetch(..)`) and
  passes its answer back as it came.

API: any method on `/{object}/…?status=N`; `/{object}/relay/{other}/…`.

Test: `echo_maps_requests_and_responses_both_ways` in
[crates/nanovm/tests/examples.rs](../../crates/nanovm/tests/examples.rs).
