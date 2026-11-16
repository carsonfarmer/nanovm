//! A shop with carts, inventory and orders on SQLite. Each shop is an
//! object (`/{shop}/`) with its own database. A checkout takes the stock,
//! writes the order, empties the cart and then charges the card; if the
//! charge fails, the handler returns an error and every one of those writes
//! rolls back with the request, so no stock is lost to an order that was
//! never paid. The object `payments` plays the card processor.
//!
//! - `PUT /{shop}/products/{sku}` `{"name":"Widget","price":250,"stock":5}` (cents)
//! - `GET /{shop}/products`
//! - `PUT /{shop}/carts/{cart}/items/{sku}` `{"qty":2}` (0 removes it)
//! - `GET /{shop}/carts/{cart}`: its items and total
//! - `POST /{shop}/carts/{cart}/checkout` `{"card":"4242…"}`: the order, or
//!   `409` with what is short
//! - `GET /{shop}/orders/{id}`

use std::cell::Cell;

use nanovm_sdk::http::header::CONTENT_TYPE;
use nanovm_sdk::http::{Request, Response};
use nanovm_sdk::{Body, Error, RequestExt, Result, kv, object, serde_json, sqlite};
use serde::Deserialize;
use serde_json::json;

nanovm_sdk::serve!(handle);

#[derive(Deserialize)]
struct Product {
    name: String,
    price: i64,
    stock: i64,
}

async fn handle(req: Request<Body>) -> Result {
    if nanovm_sdk::id() == "payments" {
        return payments(&req).await;
    }
    schema().await?;
    let path: Vec<&str> = req.uri().path().trim_matches('/').split('/').collect();
    Ok(match (req.method().as_str(), path.as_slice()) {
        ("PUT", ["products", sku]) => match req.body().json::<Product>() {
            Ok(p) if p.price >= 0 && p.stock >= 0 && !p.name.is_empty() => {
                sqlite::execute(
                    "INSERT INTO products VALUES (?, ?, ?, ?) ON CONFLICT (sku) \
                     DO UPDATE SET name = excluded.name, price = excluded.price, stock = excluded.stock",
                    &[sku, &p.name, &p.price, &p.stock],
                )
                .await?;
                Response::builder().status(204).body(Body::empty())?
            }
            _ => text(400, "{\"name\", \"price\" (cents), \"stock\"}, not negative\n")?,
        },
        ("GET", ["products"]) => json(200, sqlite::query("SELECT * FROM products ORDER BY sku", &[]).await?.json().to_string())?,
        ("PUT", ["carts", cart, "items", sku]) => {
            let qty = req.body().json::<serde_json::Value>().ok().and_then(|v| v["qty"].as_i64()).filter(|q| (0..=1000).contains(q));
            let Some(qty) = qty else { return text(400, "{\"qty\": 0 to 1000}\n") };
            if sqlite::query("SELECT 1 FROM products WHERE sku = ?", &[sku]).await?.is_empty() {
                return text(404, "not found");
            }
            match qty {
                0 => sqlite::execute("DELETE FROM cart_items WHERE cart = ? AND sku = ?", &[cart, sku]).await?,
                _ => sqlite::execute(
                    "INSERT INTO cart_items VALUES (?, ?, ?) ON CONFLICT (cart, sku) DO UPDATE SET qty = excluded.qty",
                    &[cart, sku, &qty],
                )
                .await?,
            };
            json(200, cart_json(cart).await?.to_string())?
        }
        ("GET", ["carts", cart]) => json(200, cart_json(cart).await?.to_string())?,
        ("POST", ["carts", cart, "checkout"]) => checkout(cart, &req).await?,
        ("GET", ["orders", id]) => {
            let order = sqlite::query("SELECT * FROM orders WHERE id = ?", &[id]).await?;
            let items = sqlite::query("SELECT sku, qty, price FROM order_items WHERE order_id = ?", &[id]).await?;
            match order.json().as_array().and_then(|o| o.first().cloned()) {
                Some(mut order) => {
                    order["items"] = items.json();
                    json(200, order.to_string())?
                }
                None => text(404, "not found")?,
            }
        }
        _ => text(404, "not found")?,
    })
}

async fn checkout(cart: &str, req: &Request<Body>) -> Result {
    let card = req.body().json::<serde_json::Value>().ok().and_then(|v| v["card"].as_str().map(String::from));
    let Some(card) = card else { return text(400, "{\"card\": \"…\"}\n") };
    let items = sqlite::query(
        "SELECT c.sku, c.qty, p.price, p.stock FROM cart_items c JOIN products p USING (sku) WHERE c.cart = ?",
        &[&cart],
    )
    .await?;
    if items.is_empty() {
        return text(400, "the cart is empty\n");
    }
    // Checked before anything is written, so this answer leaves nothing
    // behind. No other request can take stock in between: the shop serves
    // one request at a time.
    let mut short = vec![];
    for item in items.iter() {
        if item.get::<i64>("qty")? > item.get::<i64>("stock")? {
            short.push(item.get::<String>("sku")?);
        }
    }
    if !short.is_empty() {
        return json(409, json!({ "error": "out of stock", "short": short }).to_string());
    }

    // The writes, as one statement: placing the order fires the trigger
    // `place` (in `schema`), which takes the stock, copies the items and
    // empties the cart. Each statement that writes costs SQLite's syncs,
    // timed against the request's CPU budget. From here on a failure must
    // roll back, so it is returned as an error, never answered.
    let total: i64 = items.iter().map(|i| Ok(i.get::<i64>("qty")? * i.get::<i64>("price")?)).sum::<Result<i64>>()?;
    sqlite::execute("INSERT INTO orders (cart, total, status) VALUES (?, ?, 'paid')", &[&cart, &total]).await?;
    let order = sqlite::last_insert_rowid().await?;

    // Last, the charge. Declined, the error rolls back the stock, the order
    // and the emptied cart. The order's key makes a retried charge safe.
    let key = format!("{}/{order}", nanovm_sdk::id());
    let charge = json!({ "key": key, "card": card, "amount": total }).to_string();
    let reply = object("payments").post("/charges", charge).await?;
    if reply.status() != 200 {
        return Err(Error::new(format!("payment declined: {}", String::from_utf8_lossy(reply.body()))));
    }
    json(201, json!({ "order": order, "total": total }).to_string())
}

async fn cart_json(cart: &str) -> Result<serde_json::Value> {
    let items = sqlite::query(
        "SELECT c.sku, p.name, c.qty, p.price FROM cart_items c JOIN products p USING (sku) WHERE c.cart = ? ORDER BY c.sku",
        &[&cart],
    )
    .await?;
    let total: i64 = items.iter().map(|i| Ok(i.get::<i64>("qty")? * i.get::<i64>("price")?)).sum::<Result<i64>>()?;
    Ok(json!({ "items": items.json(), "total": total }))
}

/// A stand-in card processor: declines cards ending in `0002`, and charges
/// each idempotency key once. `GET /payments/charges?key=…` shows a charge.
async fn payments(req: &Request<Body>) -> Result {
    if req.method() == "GET" {
        let charge = kv::get_text(&format!("charge:{}", req.query("key").unwrap_or_default())).await?;
        return match charge {
            Some(charge) => json(200, charge),
            None => text(404, "not found"),
        };
    }
    let charge: serde_json::Value = req.body().json()?;
    if charge["card"].as_str().is_none_or(|c| c.ends_with("0002")) {
        return text(402, "card declined");
    }
    let key = format!("charge:{}", charge["key"].as_str().unwrap_or_default());
    if !kv::exists(&key).await? {
        kv::set(&key, charge.to_string()).await?;
    }
    json(200, kv::get_text(&key).await?.unwrap_or_default())
}

thread_local! {
    static READY: Cell<bool> = const { Cell::new(false) };
}

async fn schema() -> Result<()> {
    if READY.get() {
        return Ok(());
    }
    for table in [
        "TABLE products (sku TEXT PRIMARY KEY, name TEXT NOT NULL, price INTEGER NOT NULL, stock INTEGER NOT NULL CHECK (stock >= 0))",
        "TABLE cart_items (cart TEXT, sku TEXT REFERENCES products, qty INTEGER NOT NULL, PRIMARY KEY (cart, sku))",
        "TABLE orders (id INTEGER PRIMARY KEY, cart TEXT NOT NULL, total INTEGER NOT NULL, status TEXT NOT NULL)",
        "TABLE order_items (order_id INTEGER REFERENCES orders, sku TEXT, qty INTEGER, price INTEGER)",
        "TRIGGER place AFTER INSERT ON orders BEGIN \
           UPDATE products SET stock = stock - (SELECT qty FROM cart_items WHERE cart = NEW.cart AND sku = products.sku) \
             WHERE sku IN (SELECT sku FROM cart_items WHERE cart = NEW.cart); \
           INSERT INTO order_items SELECT NEW.id, c.sku, c.qty, p.price FROM cart_items c JOIN products p USING (sku) \
             WHERE c.cart = NEW.cart; \
           DELETE FROM cart_items WHERE cart = NEW.cart; \
         END",
    ] {
        let (kind, rest) = table.split_once(' ').expect("a kind and a definition");
        sqlite::execute(&format!("CREATE {kind} IF NOT EXISTS {rest}"), &[]).await?;
    }
    READY.set(true);
    Ok(())
}

/// A JSON answer.
fn json(status: u16, body: impl Into<Body>) -> Result {
    Ok(Response::builder().status(status).header(CONTENT_TYPE, "application/json").body(body.into())?)
}

/// A plain-text answer.
fn text(status: u16, body: impl Into<Body>) -> Result {
    Ok(Response::builder().status(status).header(CONTENT_TYPE, "text/plain; charset=utf-8").body(body.into())?)
}
