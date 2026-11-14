# Reminders, without alarms

Reminders delivered on time without anything running between requests, in
about 150 lines of Rust ([src/lib.rs](src/lib.rs)) on
[nanovm-sdk](../../sdk/rust).

```sh
tools/build-app.sh examples/reminders -o examples/target/reminders.wasm --run --manifest examples/reminders/nanovm.json
```

```sh
curl -XPOST http://127.0.0.1:8080/ann/reminders -d '{"text":"tea","in_ms":60000}'
# an external clock, here a shell loop (in production: cron, a CI schedule, an uptime checker):
while sleep 60; do curl -s -XPOST http://127.0.0.1:8080/clock/tick; done
curl http://127.0.0.1:8080/ann/due   # or poll from the client
```

What it shows, honestly:

- **nanovm has no alarms.** An object runs only when a request comes; it
  can't ask to be woken at a time of its choosing. Until it can, there are
  two ways to act on time, and this app has both.
- **An external clock.** A `clock` object keeps every reminder's due time as
  a key that sorts by time. Something outside sends `POST /clock/tick`
  every minute; the clock calls the owner's object for each due reminder,
  and that object pushes it to its owner's WebSockets. Delivery is late by
  up to the tick interval, and stops if the outside clock stops. A call that
  fails stays due for the next tick.
- **Client polling.** `GET /{user}/due` hands over what is due when the
  client asks: no clock at all, but nothing is delivered while no one asks.
- **Exactly once, by the owner.** Whichever comes first, tick or poll, marks
  the reminder fired in its owner's object, which serves one request at a
  time; two ticks at once fire it once.
- **Two objects, two commits.** Making a reminder writes it in the owner's
  object and schedules it with a call to the clock, which commits on its
  own first. If the owner's request then failed, the clock would later ask
  about a reminder that doesn't exist, be told `404`, and drop it.

API: `POST /{user}/reminders` `{"text","in_ms"|"at"}` → `201` the reminder;
`GET /{user}/reminders`; `GET /{user}/due` → the reminders now due, marked
fired; `/{user}/ws` → each reminder as it fires; `POST /clock/tick` →
`{"fired"}`. A reminder is `{"id","text","at","fired"}`.

Test: `reminders_fire_once_on_a_tick_or_a_poll` in
[crates/nanovm/tests/examples.rs](../../crates/nanovm/tests/examples.rs).
