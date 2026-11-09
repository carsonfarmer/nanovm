# Leaderboard

Leaderboards on SQLite, a database per board, in about 100 lines of Rust
([src/lib.rs](src/lib.rs)) on [nanovm-sdk](../../sdk/rust).

```sh
export WASI_SDK_PATH=/path/to/wasi-sdk-34.0-x86_64-linux   # SQLite is C
tools/build-app.sh examples/leaderboard -o examples/target/leaderboard.wasm --run --manifest examples/leaderboard/nanovm.json
```

```sh
curl -XPOST http://127.0.0.1:8080/arcade/scores -d '[{"player":"ann","score":120},{"player":"bob","score":90}]'
curl 'http://127.0.0.1:8080/arcade/top?limit=10&offset=0'   # [{"player":"ann","rank":1,"score":120},…]
curl http://127.0.0.1:8080/arcade/players/bob               # {"player":"bob","rank":2,"score":90}
```

What it shows:

- **SQLite per object.** Each board (`/{board}/`) has its own database in
  its own volume (`nanovm_sdk::sqlite`, the provider composed in by
  `tools/build-app.sh`): no shared database to shard or migrate, and an
  index serves the top of the board and any player's rank.
- **The request is the transaction.** `BEGIN` and `COMMIT` are refused; a
  request's statements commit together when it answers and roll back
  together if it fails.
- **Write a batch in one statement.** Every statement that writes is a
  SQLite transaction with its own syncs, and the time SQLite waits on them
  counts against the request's CPU budget. Inserting rows one statement at
  a time cost about 6-12 ms each on a busy machine (a batch of 40 ran out of
  the 200 ms budget); the whole batch as one
  `INSERT … SELECT … FROM json_each(?)` takes 20-50 ms for 100 rows.
- **Pages with `LIMIT`/`OFFSET`** and competition ranking (`rank()`):
  tied scores share a rank; their order goes to whoever got there first.

API: `POST /{board}/scores` with `{"player","score"}` or an array of up to
100 (a player is 1-64 characters, a score an integer; a player keeps their
best) → their standings `[{"player","score","rank"}]`; `GET
/{board}/top?limit=10&offset=0` (at most 100) → `[{"rank","player","score"}]`;
`GET /{board}/players/{player}` → `{"player","score","rank"}` or `404`;
`DELETE /{board}/players/{player}` → `204`.

Test: `leaderboard_pages_and_ranks_from_sqlite` in
[crates/nanovm/tests/examples.rs](../../crates/nanovm/tests/examples.rs).
