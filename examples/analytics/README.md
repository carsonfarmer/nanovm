# Analytics collector

Event ingest with per-day rollups and CSV export, in about 110 lines of Rust
([src/lib.rs](src/lib.rs)) on [nanovm-sdk](../../sdk/rust).

```sh
export WASI_SDK_PATH=/path/to/wasi-sdk-34.0-x86_64-linux   # SQLite is C
tools/build-app.sh examples/analytics -o examples/target/analytics.wasm --run --manifest examples/analytics/nanovm.json
```

```sh
curl -XPOST http://127.0.0.1:8080/blog/events -d '[{"name":"view","path":"/","user":"u1"},{"name":"click","user":"u1"}]'
curl http://127.0.0.1:8080/blog/days        # today: [{"day","name","events","users"},…]
curl -O 'http://127.0.0.1:8080/blog/days.csv?from=2026-11-01&to=2026-11-30'
```

What it shows:

- **A database per site.** Each site (`/{site}/`) is an object with its own
  SQLite database, so one busy site never slows another and there is no
  shared events table to partition.
- **Rollups kept as events arrive.** A trigger adds each event to its day's
  count in the same statement that stores it, so a month of counts is a
  month of rows, not a scan of every event; distinct users come from an
  index on the raw events.
- **Batches are one statement.** Up to 100 events are inserted with
  `INSERT … SELECT … FROM json_each(?)`: one SQLite transaction, one set of
  syncs, however many events (see N-600 for why that matters).
- **Late events land on their day.** An event may carry its own `at` (within
  the last 30 days), for clients that queue events offline.

API: `POST /{site}/events` with `{"name","path","user","at"?}` or an array of
up to 100 (a name is 1-64 of `A-Za-z0-9_.-`) → `202 {"accepted"}`; `GET
/{site}/days?from=&to=` (dates `YYYY-MM-DD`, UTC; today by default) →
`[{"day","name","events","users"}]`; `GET /{site}/days.csv?from=&to=`.

Test: `analytics_rolls_events_up_per_day` in
[crates/nanovm/tests/examples.rs](../../crates/nanovm/tests/examples.rs).
