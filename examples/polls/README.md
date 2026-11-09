# Polls

Polls with one vote per voter and live results, in about 100 lines of Rust
([src/lib.rs](src/lib.rs)) on [nanovm-sdk](../../sdk/rust).

```sh
tools/build-app.sh examples/polls -o examples/target/polls.wasm --run --manifest examples/polls/nanovm.json
```

```sh
curl -XPUT http://127.0.0.1:8080/lunch/ -d '{"question":"Lunch?","options":["pizza","sushi"]}'
curl -c jar -b jar -XPOST 'http://127.0.0.1:8080/lunch/vote?option=0'   # sets a voter cookie
curl -c jar -b jar -XPOST 'http://127.0.0.1:8080/lunch/vote?option=1'   # moves that vote
curl http://127.0.0.1:8080/lunch/   # {"question":"Lunch?","options":[{"text":"pizza","votes":0},{"text":"sushi","votes":1}],"voters":1}
```

A WebSocket on `ws://127.0.0.1:8080/lunch/ws` gets the results, then the
new results after every vote.

What it shows:

- **Dedupe without a unique index.** Each poll is an object (`/{poll}/`).
  A voter's choice is one key (`vote:{voter}`) and the tally another; a vote
  reads the voter's old choice and moves it, and since an object serves one
  request at a time, twenty voters at once each count exactly once.
- **Live results only for what committed.** A vote publishes the new
  results to the channel its watchers subscribed to; the publish goes out
  only once the vote has committed, so a watcher never sees a vote that was
  lost.
- **Sockets outlive instances.** Watchers stay connected while the poll's
  instance is evicted and started again; the next vote still reaches them.
- **Cookies.** A voter is a browser with a random `voter` cookie
  (`Request::cookie`), set on its first vote. That stops double voting from
  one browser, not a determined voter: real polls need sign-in.

Each voter is a file in the poll's volume, so the manifest's `max_files`
bounds the voters a poll can have; a poll with millions of voters would keep
them in SQLite.

API: `PUT /{poll}/` `{"question","options":[…]}` (1-200 characters, 2-10
options of 1-100) → `201`, or `409` if it exists; `POST /{poll}/vote?option=N`
→ the results with `you`, and `set-cookie` for a new voter; `GET /{poll}/` →
`{"question","options":[{"text","votes"}],"voters","you"?}`; `/{poll}/ws`.

Test: `polls_count_each_voter_once_and_push_results` in
[crates/nanovm/tests/examples.rs](../../crates/nanovm/tests/examples.rs).
