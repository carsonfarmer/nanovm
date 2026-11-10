# Time series with hour and day rollups

A Wasm component for numeric time series (metrics, sensor readings, counters)
in the object's own `/data` volume: raw points, per-hour and per-day rollups
(count, sum, min, max) kept up to date on every write, aggregation at any step,
and retention per level. The host knows nothing about it: compose it with
`wac plug`, like the [KV](../keyvalue/README.md) and [SQLite](../sqlite/README.md)
providers.

```sh
bash components/timeseries/build.sh   # target/components/timeseries/{provider,tests}.wasm
wac plug app.wasm --plug target/components/timeseries/provider.wasm -o app-with-ts.wasm
```

The interface is [`nanovm:timeseries/series@0.1.0`](wit/timeseries.wit):
`series.open`, `write` (1-10,000 points, any order), `range` (raw points),
`aggregate(from, to, step)`, `latest`, and `trim(level, before)`. No standard
WIT exists for time series.

## Storage

`/data/.ts/<series>/`:

- `r<day>`: one UTC day's raw points, 16 bytes each, in write order (a read
  sorts them; equal times keep write order). Writes append, so the host's undo
  journal records the old size, not data.
- `h<n>`: hour rollups for 32 days (768 slots of 32 bytes), `d<n>`: day
  rollups for 1,024 days. A write updates each slot it touches in place; the
  journal keeps the 32 old bytes. Files grow only as far as the last slot
  written.

`aggregate` reads day rollups when the step is a whole number of days, hour
rollups when it is a whole number of hours (both still work after raw points
are trimmed, and count whole hours/days at the ends of the range), and raw
points otherwise. Buckets are aligned to the epoch.

## Why it needs no write-ahead log

nanovm commits all of a request's changes to `/data` together, or rolls them
all back. A write touches up to three files per day it covers, so it keeps an
undo list in memory for that call only: if the quota refuses any step, it puts
back what the earlier steps changed (truncates appends, restores slots,
removes new files) and returns `full` with nothing changed. If an undo fails,
it traps and the whole request rolls back. The list never touches disk: the
host's per-request commit is the durable boundary.

## Limits

- Series names: 1-64 ASCII letters, digits, `_` or `-`. Times: 0 to 2^53 ms;
  values: finite f64 (sums are f64 too).
- `range` returns at most 10,000 points; continue from the last time + 1
  (points sharing that exact time past the limit are skipped).
- `aggregate` covers at most 10,000 buckets of the step.
- A raw read loads whole days: a day at one point per second is 1.4 MB.
- `trim` removes whole files only (a day of raw points, 32 days of hour
  rollups, 1,024 days of day rollups); it may stop early if the host refuses an
  unlink and returns `ok`, and a later call continues.
- Files: one per day of raw points, plus one per 32 days and per 1,024 days
  of rollups, against `max_files`. Trim raw points to stay within it.
- Cooperative ownership: don't write `/data/.ts` directly.

## Cost

See [notes/components.md](../../notes/components.md). A point costs 16 bytes
raw; rollups add 32 bytes per hour and per day that has data.

## Tests

`crates/nanovm/tests/components.rs`, `timeseries_*`: contracts (order, equal
times, day boundaries, every step kind), rollups across eviction, restart and
raw trim, rollback of failed requests, and quota exhaustion (a refused batch
leaves raw points and both rollups agreeing; a batch refused for want of a
directory entry also undoes its append to an existing day). Disabling the undo
list fails `timeseries_quota_exhaustion_is_refused_cleanly`.
