# JavaScript apps

A nanovm app can be a Workers-style ES module. `js/` is a runtime component
that embeds QuickJS (QuickJS-NG through rquickjs) and runs the app's module;
it is built on [nanovm-sdk](../sdk/rust) and adds nothing to the node.

```js
// js/examples/counter.js: each object (URL /{id}/…) has its own count.
export default {
  async fetch(request, env) {
    let n = (await env.kv.get("n")) ?? 0;
    if (request.method === "POST") await env.kv.put("n", ++n);
    return Response.json({ object: env.id, n });
  },
};
```

```sh
export WASI_SDK_PATH=/path/to/wasi-sdk-34.0-x86_64-linux   # QuickJS is C
tools/build-app.sh js/examples/counter.js --run             # counter.wasm, served on 127.0.0.1:8080
curl -XPOST http://127.0.0.1:8080/alice/                    # {"object":"alice","n":1}
```

`tools/build-app.sh` compiles the runtime with the app in it, pre-initializes
it with Wizer (the component starts with the engine up and the module
evaluated; every host import traps meanwhile, so no handle, clock or
randomness is baked in), then composes the key-value provider. The first
build compiles the runtime (minutes); each app after that takes seconds.

**What works:** `fetch(request, env, ctx)` with `Request`, `Response`
(`json`, `redirect`), `Headers`, `URL`, `URLSearchParams`,
`TextEncoder`/`TextDecoder`, `structuredClone`, `atob`/`btoa`, `console`
(to the object's log), timers, `queueMicrotask`, `crypto.getRandomValues`
and `randomUUID`; `env.kv` (`get`, `put`, `delete`, `list`; values are
JSON) on the composed Spin key-value provider, committed with the response;
outbound `fetch` under the app's egress policy, within the node operator's
(it rejects if the body breaks off before its end); `env.objects.get(id).fetch()`
between objects; `env.config` (the app's config and its
[secrets](../README.md#secrets)), `env.id`, `env.publish(channel, message)`;
`ctx.waitUntil(promise)` (its work starts when the promise is made: what it
writes before the handler answers commits with the answer. The object
then waits for every waitUntil promise, those registered meanwhile
included, before its next request; what they write after the answer
commits once all have settled. If one rejects, whenever it does, or they
can never settle, that is logged (`Uncaught (in waitUntil) …`); if they
outlive `post_return_timeout_ms`, the node logs the deadline. Either way
the writes after the answer roll back and the instance is replaced; the
answer and what was written before it stand. This is Rust's `after`, in
the main README's "Commits"); WebSocket handlers `webSocketOpen` (return
`false` to refuse), `webSocketMessage` and `webSocketClose`, with `ws.send`,
`ws.close(code)` and `ws.subscribe(channel)`. An uncaught exception fails
the request and rolls back its writes. Bodies are whole (no streams), one
module (bundle imports first), no `performance`.

Tests: `crates/nanovm/tests/js.rs` (seventeen, on an in-process node) and
`javascript_apps_run_and_publish` in `crates/nanovm/tests/run.rs`.
Measurements: notes/js.md and notes/qjs.md on branch cf/optimistic-feynman.
