# Game lobby: tic-tac-toe

Matchmaking and live games, each game an object, in about 150 lines of Rust
([src/lib.rs](src/lib.rs)) on [nanovm-sdk](../../sdk/rust).

```sh
tools/build-app.sh examples/tictactoe -o examples/target/tictactoe.wasm --run --manifest examples/tictactoe/nanovm.json
```

```sh
curl -XPOST 'http://127.0.0.1:8080/lobby/match?player=ann'   # {"game":"g-1a2b3c4d","you":"x"}
curl -XPOST 'http://127.0.0.1:8080/lobby/match?player=bob'   # {"game":"g-1a2b3c4d","you":"o"}
curl -XPOST 'http://127.0.0.1:8080/g-1a2b3c4d/move?player=ann&square=4'
```

In a browser, `new WebSocket("ws://127.0.0.1:8080/g-1a2b3c4d/ws?player=ann")`
gets the game after every move and sends a square (`0`-`8`) to play it.

What it shows:

- **Rooms as objects.** Each game (`/g-…/`) holds its board in its own
  key-value store, decides every move itself (whose turn, which square,
  whether the game is over) and pushes the board to its players' sockets.
  Nothing outside the game can change it except by asking it.
- **Matchmaking through calls.** The `lobby` object keeps the one waiting
  game; the next player joins it with a call to the game
  (`object(game).post("/join", …)`), or starts a new one. The lobby is one
  request at a time, so ten players matching at once make five games of
  exactly two.
- **Sockets outlive instances.** A game's instance can be evicted mid-game;
  its players' sockets stay with the proxy, and the next move starts it
  again.

The gap: a game can't tell the lobby's calls from a client's requests (no
caller identity on calls, W-602), so its lobby routes (`/new`, `/join`) are
public. They are harmless here: a game is made once, its second seat filled
once, and the lobby copes with a seat taken behind its back. Players are
names, taken on trust; a real game signs them in.

API: `POST /lobby/match?player=…` → `{"game","you"}`; `/{game}/ws?player=…`;
`POST /{game}/move?player=…&square=N` → the game, or `409 {"error"}`; `GET
/{game}/` → `{"x","o","board","turn","result"}` (`board`: nine of `x`, `o`,
`.`; `result`: `x`, `o`, `draw` or `null`).

Test: `tictactoe_lobby_pairs_players_and_games_judge_moves` in
[crates/nanovm/tests/examples.rs](../../crates/nanovm/tests/examples.rs).
