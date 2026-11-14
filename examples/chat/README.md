# Chat

Live multi-room chat in about 100 lines of Rust ([src/lib.rs](src/lib.rs)) and a
page ([src/index.html](src/index.html)), built with
[nanovm-sdk](../../sdk/rust).

```sh
tools/build-app.sh examples/chat -o examples/target/chat.wasm --run --manifest examples/chat/nanovm.json
```

Then open <http://127.0.0.1:8080/lobby/>, pick a room, and open it in a second
window. Rebuilding (the same command without `--run`) restarts the running app.

- **Each room is an object** (`/{room}/`). Its history is a file in its
  volume, so it survives eviction and restarts; it keeps nothing in memory.
- **Members are WebSockets.** A socket joins its room's channel on `OPEN` and
  gets the history; each message it sends is stored, then published to the
  channel. The publish goes out only if storing it committed.
- **The lobby is an object too.** Each room tells it about every message with
  a call between objects (`object("lobby").post(..)`); the lobby keeps one
  key per room in its key-value store (the composed provider) and publishes
  the change to its own sockets, so the room list updates live. Anyone can
  post to the lobby's URL, so it parses what it gets and writes it afresh
  rather than storing it as sent.

`curl` works as well as a browser:

```sh
curl -XPOST 'http://127.0.0.1:8080/general/messages?name=ann' -d 'hello'
curl http://127.0.0.1:8080/general/messages
curl http://127.0.0.1:8080/lobby/rooms
```

The test `the_chat_example_is_live_and_durable` (crates/nanovm/tests/run.rs)
runs this example under `nanovm run`: two members, a message over HTTP, the
lobby's index and its live updates, eviction under open sockets, a restart,
and refused bad input.
