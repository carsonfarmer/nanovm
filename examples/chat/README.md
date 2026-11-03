# Chat

Live multi-room chat in about 100 lines of Rust ([src/lib.rs](src/lib.rs)) and a
page ([src/index.html](src/index.html)), built with
[nanovm-sdk](../../guests/sdk).

```sh
examples/chat/run.sh
```

Then open <http://127.0.0.1:8080/lobby/>, pick a room, and open it in a second
window. The script builds the component and `nanovm`, then runs
`nanovm run guests/target/wasm32-wasip2/release/chat.wasm --manifest examples/chat/nanovm.json --data target/chat-data`.

- **Each room is an object** (`/{room}/`). Its history is a file in its
  volume, so it survives eviction and restarts; it keeps nothing in memory.
- **Members are WebSockets.** A socket joins its room's channel on `OPEN` and
  gets the history; each message it sends is stored, then published to the
  channel. The publish goes out only if storing it committed.
- **The lobby is an object too.** Each room tells it about every message with
  a call between objects (`object("lobby").post(..)`); the lobby keeps one
  key per room and publishes the change to its own sockets, so the room list
  updates live. Anyone can post to the lobby's URL, so it parses what it gets
  and writes it afresh rather than storing it as sent.

`curl` works as well as a browser:

```sh
curl -XPOST 'http://127.0.0.1:8080/general/messages?name=ann' -d 'hello'
curl http://127.0.0.1:8080/general/messages
curl http://127.0.0.1:8080/lobby/rooms
```

The integration test `run::the_chat_example_is_live_and_durable` runs this
example under `nanovm run`: two members, a message over HTTP, the lobby's
index and its live updates, eviction under open sockets, a restart, and
refused bad input.
