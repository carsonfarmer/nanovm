# Presence and typing indicators

Who is in a room and who is typing, over WebSockets, in about 80 lines of
Rust ([src/lib.rs](src/lib.rs)) on [nanovm-sdk](../../sdk/rust).

```sh
tools/build-app.sh examples/presence -o examples/target/presence.wasm --run --manifest examples/presence/nanovm.json
```

```js
const ws = new WebSocket("ws://127.0.0.1:8080/lounge/ws?name=ann");
ws.onmessage = (e) => console.log(JSON.parse(e.data)); // here, join, leave, typing
input.oninput = () => ws.send("typing");
setInterval(() => ws.send("ping"), 20000);
```

`curl http://127.0.0.1:8080/lounge/who` gives `{"names":["ann"]}`.

What it shows:

- **Socket lifecycles are requests.** The proxy turns a socket's open,
  messages and close into requests to the room's object, one at a time with
  everything else it serves; the room keeps one key per socket (named by the
  proxy's `connection-id`) and publishes `join`, `leave` and `typing` to its
  channel. Two tabs are one person: closing one says nothing.
- **Typing is not stored.** It is published and forgotten; only presence is
  kept, so it survives the room's instance being evicted under its open
  sockets.
- **Sockets that die silently.** A socket that closes says so, but when the
  proxy or node holding it goes down (here: a restart of `nanovm run`), no
  `DISCONNECT` ever arrives. So presence carries a time: a socket not heard
  from (`ping` or `typing`) within `stale_ms` (config) lapses, and is
  dropped the next time anyone asks who is here.
- **Refusing a socket with a status.** A socket without a name gets `400`:
  answering `OPEN` with an ordinary response refuses it with that status.

API: `/{room}/ws?name=…` (1-32 characters) → `{"type":"here","names"}`, then
`{"type":"join"|"leave"|"typing","name"}`; it sends `typing` and `ping`.
`GET /{room}/who` → `{"names":[…]}`.

Test: `presence_tracks_who_is_here` in
[crates/nanovm/tests/examples.rs](../../crates/nanovm/tests/examples.rs).
