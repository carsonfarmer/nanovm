# Static site plus API

A site's files and its API in one component, in about 50 lines of Rust
([src/lib.rs](src/lib.rs)) and three files in [src/public](src/public), on
[nanovm-sdk](../../sdk/rust).

```sh
tools/build-app.sh examples/site -o examples/target/site.wasm --run --manifest examples/site/nanovm.json
```

Then open <http://127.0.0.1:8080/www/> and press the button.

What it shows:

- **Assets are part of the app.** `include_bytes!` embeds the page, its
  stylesheet and its script, so a deploy is one signed component and the
  site can never be out of step with its API.
- **Caching that saves work.** Each file has an `ETag` (a hash of its bytes,
  the same across restarts and rebuilds of the same bytes); a client that
  sends it back in `If-None-Match` gets `304` and no body. The page is
  `Cache-Control: no-cache` (checked each visit, since it names the other
  files); the stylesheet and script may be kept an hour. The API is
  `no-store`. `HEAD` gives the headers and length without the body.
- **The API is the same object.** `/{object}/api/likes` keeps its count in
  the object's key-value store; the page fetches it with relative URLs, so
  the site works under any object's path.

The gap: every URL names an object first, so a site lives at `/www/`, not at
`/`; the node answers `/` with `400 bad-object-id` (W-601).

API: `GET /{object}/`, `/{object}/style.css`, `/{object}/app.js` (and
`HEAD`; `If-None-Match`); `GET`/`POST /{object}/api/likes` → `{"likes"}`.

Test: `site_serves_embedded_files_and_its_api` in
[crates/nanovm/tests/examples.rs](../../crates/nanovm/tests/examples.rs).
