# Job queue

A job queue with leases, retries, idempotency keys and dead letters, on
SQLite, in about 170 lines of Rust ([src/lib.rs](src/lib.rs)) on
[nanovm-sdk](../../sdk/rust).

```sh
export WASI_SDK_PATH=/path/to/wasi-sdk-34.0-x86_64-linux   # SQLite is C
tools/build-app.sh examples/jobs -o examples/target/jobs.wasm --run --manifest examples/jobs/nanovm.json
```

```sh
curl -XPOST http://127.0.0.1:8080/mail/jobs -H 'idempotency-key: welcome-ann' -d '{"text":"hi ann"}'
curl -XPOST 'http://127.0.0.1:8080/mail/lease?visibility_ms=30000'   # {"id":1,"lease":"9f…","attempts":1,…}
curl -XPOST 'http://127.0.0.1:8080/mail/jobs/1/ack?lease=9f…' -d 'sent'
curl -XPOST 'http://127.0.0.1:8080/worker-1/work?queue=mail'          # a worker object does the rest
curl http://127.0.0.1:8080/mail/                                       # {"ready":0,"leased":0,"done":1,"dead":0}
```

What it shows:

- **Visibility timeouts without timers.** Nothing runs between requests, so
  a lease is a time in the job's row, and the next `lease` request treats a
  lease that ran out as visible again. A job whose last attempt's lease runs
  out goes to the dead letters the same way.
- **Fenced acks.** Each lease has a random token; only the latest lease can
  ack or nack its job, so a worker that stalled past its lease can't
  overwrite the work of the one that took over.
- **Idempotent enqueue.** A producer that retries sends the same
  `idempotency-key` and gets the same job back (`200` instead of `201`),
  whatever has happened to it since.
- **Workers pull through calls.** `POST /{worker}/work?queue=…` makes any
  other object a worker: it leases jobs from the queue object with calls
  between objects, does them (here: capitals; a job with `"fail": true`
  fails), and acks with the result, so a result is stored in the same
  commit as its ack and a worker keeps nothing it could lose. A call the
  queue failed to commit (`5xx`) is retried.
- **One writing statement per request** (N-600): leasing is one
  `UPDATE … RETURNING`, acks and nacks one `UPDATE` each.

API: `POST /{queue}/jobs` (JSON body; optional `idempotency-key`) → `201`
the job, or `200` the existing one; `POST /{queue}/lease?visibility_ms=30000`
→ the job with `lease` and `attempts`, or `204`; `POST
/{queue}/jobs/{id}/ack?lease=…` (body: result) and `…/nack?lease=…&delay_ms=1000`
(body: error) → `204`, or `409` if the lease isn't the job's any more;
`GET /{queue}/jobs/{id}`; `GET /{queue}/` → counts by state; `GET
/{queue}/dead`; `POST /{queue}/dead/{id}/retry`. A job is
`{"id","payload","state","attempts","lease","visible_at","result","error"}`;
`max_attempts` (default 3) is the manifest's config.

Test: `jobs_lease_retry_and_dead_letter` in
[crates/nanovm/tests/examples.rs](../../crates/nanovm/tests/examples.rs).
