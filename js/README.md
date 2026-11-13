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
outbound `fetch` under the app's egress policy (it rejects if the body
breaks off before its end); `env.objects.get(id).fetch()`
between objects; `env.config`, `env.id`, `env.publish(channel, message)`;
`ctx.waitUntil` (its work runs after the answer and commits when done; if
a promise rejects, the instance stops and that work's writes roll back,
while the answer and the handler's own writes stand, as with Rust's
`after`); WebSocket handlers `webSocketOpen` (return `false` to
refuse), `webSocketMessage` and `webSocketClose`, with `ws.send`,
`ws.close(code)` and `ws.subscribe(channel)`. An uncaught exception fails
the request and rolls back its writes. Bodies are whole (no streams), one
module (bundle imports first), no `performance`.

Tests: `crates/nanovm/tests/js.rs` (eleven, on an in-process node) and
`javascript_apps_run_and_publish` in `crates/nanovm/tests/run.rs`.
Measurements: notes/js.md and notes/qjs.md on branch cf/optimistic-feynman.
