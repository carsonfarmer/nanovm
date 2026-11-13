# Server-Sent Events

Event feeds over Server-Sent Events, and a long-lived streamed response, in
about 60 lines of Rust ([src/lib.rs](src/lib.rs)) on
[nanovm-sdk](../../sdk/rust).

```sh
tools/build-app.sh examples/events -o examples/target/events.wasm --run --manifest examples/events/nanovm.json
```

```sh
curl -XPOST http://127.0.0.1:8080/news/events -d 'hello'       # {"id":1}
curl -N http://127.0.0.1:8080/news/events                       # retry: 1000, then id: 1 / data: hello
curl -N -H 'last-event-id: 1' http://127.0.0.1:8080/news/events # only what came after 1
curl -N 'http://127.0.0.1:8080/demo/countdown?from=5&every_ms=500'
```

In a browser, `new EventSource("/news/events")` gets every event, and
reconnects a second after each response with the last id it saw.

What it shows, and what a stream costs:

- **A streamed body is written after the handler returns**
  (`Body::stream`). The countdown's ticks arrive as they are written,
  and what it changes after answering (it counts finished countdowns)
  commits when the stream ends.
- **Meanwhile the object serves nothing else.** An object is one request at
  a time, and a stream is still that request: a `POST` to the same object
  waits for the stream to end; other objects are not held up.
- **Streams are bounded by `post_return_timeout_ms`.** A stream that outlives
  it is cut (the client sees the body end early, without `done`), the
  object is stopped (`timeout: deadline exceeded while post-return`), and
  what it changed after answering rolls back. What it changed before
  answering had already committed.
- **So a feed doesn't hold a stream open waiting for news**: nothing could
  post the news while it waits. `GET /{feed}/events` sends everything after
  `Last-Event-ID` at once, with `retry: 1000`, and lets the browser come
  back. For pushes that arrive the moment they happen, use WebSockets: the
  proxy holds those, not the object (see [chat](../chat) and
  [polls](../polls)).

API: `POST /{feed}/events` (body: up to 4 KiB) → `201 {"id"}`; `GET
/{feed}/events` (`Last-Event-ID` header or `?after=N`) → `text/event-stream`;
`GET /{feed}/countdown?from=5&every_ms=200` → a stream of `tick` events, then
`done`; `GET /{feed}/` → `{"events","countdowns"}`.

Test: `events_stream_after_the_handler_returns` in
[crates/nanovm/tests/examples.rs](../../crates/nanovm/tests/examples.rs).
