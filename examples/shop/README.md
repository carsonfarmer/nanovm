# Shop: carts, inventory and checkout

A shop on SQLite where a failed checkout leaves nothing behind, in about 150
lines of Rust ([src/lib.rs](src/lib.rs)) on [nanovm-sdk](../../sdk/rust).

```sh
export WASI_SDK_PATH=/path/to/wasi-sdk-34.0-x86_64-linux   # SQLite is C
tools/build-app.sh examples/shop -o examples/target/shop.wasm --run --manifest examples/shop/nanovm.json
```

```sh
curl -XPUT http://127.0.0.1:8080/acme/products/widget -d '{"name":"Widget","price":250,"stock":5}'
curl -XPUT http://127.0.0.1:8080/acme/carts/ann/items/widget -d '{"qty":2}'
curl -XPOST http://127.0.0.1:8080/acme/carts/ann/checkout -d '{"card":"4000000000000002"}'   # declined: 500, nothing changed
curl -XPOST http://127.0.0.1:8080/acme/carts/ann/checkout -d '{"card":"4242424242424242"}'   # 201 {"order":1,"total":500}
curl http://127.0.0.1:8080/acme/orders/1
```

What it shows:

- **The request is the transaction.** A checkout takes the stock, writes
  the order and its items, empties the cart, and only then charges the card
  (a call to the object `payments`, which stands in for a card processor).
  A declined card makes the handler return an error, and every write the
  request made rolls back with it: the stock, the order, the cart. There is
  no `BEGIN`, no compensation code, and no window in which another request
  sees the stock taken.
- **Check, then write.** Stock is checked before anything is written, and
  since the shop serves one request at a time nothing can take it in
  between: five carts after the last three widgets get three orders and two
  `409`s, never an oversold `-2`. A `409` (or any answer) commits, so it is
  only given before the first write.
- **One statement that writes.** Placing the order is one `INSERT`; a
  trigger does the rest in the same SQLite statement. Each writing
  statement costs SQLite's own syncs, and the time spent waiting on them
  counts against the request's CPU budget: four separate statements plus
  the call ran out of the 200 ms budget on a busy machine, one does not.
- **Idempotent charges.** The charge carries the order as its key, so
  retrying a checkout whose answer was lost doesn't charge twice.

The gap: a rolled-back checkout can only answer `500` (`x-nanovm-error:
guest-error`, the message in its body), not `402 Payment Required`. An
answer, whatever its status, commits; only an error rolls back (W-600).

API: `PUT /{shop}/products/{sku}` `{"name","price","stock"}` (cents) →
`204`; `GET /{shop}/products`; `PUT /{shop}/carts/{cart}/items/{sku}`
`{"qty"}` (0 removes it) → the cart; `GET /{shop}/carts/{cart}` →
`{"items","total"}`; `POST /{shop}/carts/{cart}/checkout` `{"card"}` →
`201 {"order","total"}`, `409 {"short":[sku…]}`, `400` for an empty cart, or
`500` if the card is declined; `GET /{shop}/orders/{id}` → the order with
its items. `GET /payments/charges?key={shop}/{order}` shows a charge.

Test: `shop_checkout_is_all_or_nothing` in
[crates/nanovm/tests/examples.rs](../../crates/nanovm/tests/examples.rs).
