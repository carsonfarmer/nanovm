// The JavaScript test app: a counter in the object's storage plus hostile
// and API-surface routes for crates/nanovm/tests/js.rs.
let hits = 0; // module state lives as long as the instance

const routes = {
  "GET /": async (req, env) => Response.json({ object: env.id, n: (await env.kv.get("n")) ?? 0, hits }),
  "POST /incr": async (req, env) => {
    const n = ((await env.kv.get("n")) ?? 0) + 1;
    await env.kv.put("n", n);
    return Response.json({ n, hits });
  },
  // The write must roll back with the request.
  "POST /incr-then-throw": async (req, env) => {
    await env.kv.put("n", ((await env.kv.get("n")) ?? 0) + 1);
    throw new Error("boom");
  },
  "GET /spin": () => { for (;;) {} },
  "GET /alloc": () => { const a = []; for (;;) a.push(new Array(1 << 16).fill(a.length)); },
  "GET /never": () => new Promise(() => {}),
  "GET /recurse": () => { const f = (n) => f(n + 1) + 1; return f(0); },
  "GET /fetch": async (req) => {
    const url = new URL(req.url).searchParams.get("url");
    try {
      const r = await fetch(url);
      return Response.json({ status: r.status, body: await r.text() });
    } catch (e) {
      return Response.json({ denied: String(e) }, { status: 502 });
    }
  },
  "POST /call": async (req, env) => {
    const id = new URL(req.url).searchParams.get("id");
    const r = await env.objects.get(id).fetch("/incr", { method: "POST" });
    return Response.json({ status: r.status, callee: await r.json() });
  },
  "GET /config": (req, env) => Response.json(env.config),
  // The runtime sets content-length itself, whatever the app says.
  "GET /length": () => new Response("hi", { headers: { "content-length": "99" } }),
  "GET /kv": async (req, env) => Response.json(Object.fromEntries(await env.kv.list())),
  "POST /echo": async (req) => {
    const u = new URL(req.url);
    return Response.json({
      method: req.method,
      path: u.pathname,
      q: Object.fromEntries(u.searchParams),
      type: req.headers.get("content-type"),
      body: await req.text(),
    }, { status: 201, headers: { "x-echo": "1" } });
  },
  "GET /timers": async () => {
    const t0 = Date.now();
    const order = [];
    const cancelled = setTimeout(() => order.push("cancelled"), 5);
    clearTimeout(cancelled);
    await Promise.all([
      new Promise((r) => setTimeout(() => { order.push("b"); r(); }, 30)),
      new Promise((r) => setTimeout(() => { order.push("a"); r(); }, 10)),
    ]);
    let ticks = 0;
    await new Promise((r) => { const id = setInterval(() => { if (++ticks === 3) { clearInterval(id); r(); } }, 1); });
    return Response.json({ order, waited: Date.now() - t0, ticks });
  },
  "GET /later": async (req, env, ctx) => {
    ctx.waitUntil((async () => {
      await new Promise((r) => setTimeout(r, 10));
      await env.kv.put("later", hits);
    })());
    return new Response("answered first");
  },
  "GET /later-value": async (req, env) => Response.json({ later: (await env.kv.get("later")) ?? null }),
  // waitUntil work that writes, then throws: its write rolls back; the
  // answer and the handler's own write stand.
  "POST /later-throw": async (req, env, ctx) => {
    await env.kv.put("answered", hits);
    ctx.waitUntil((async () => {
      // Well after the answer: waitUntil work done while the handler's own
      // promise was still settling would commit with the answer.
      await new Promise((r) => setTimeout(r, 200));
      await env.kv.put("partial", hits);
      throw new Error("after the answer");
    })());
    return new Response("answered first");
  },
  // review2: waitUntil work that writes while the handler is running and
  // rejects after it has answered.
  "POST /later-throw-straddle": async (req, env, ctx) => {
    ctx.waitUntil((async () => {
      await env.kv.put("early", hits);
      await new Promise((r) => setTimeout(r, 200));
      throw new Error("waitUntil work failed");
    })());
    await new Promise((r) => setTimeout(r, 50));
    return new Response("answered first");
  },
  // review2: waitUntil work that registers more waitUntil work after the
  // answer; that work writes, then rejects.
  "POST /later-nested": async (req, env, ctx) => {
    ctx.waitUntil((async () => {
      await new Promise((r) => setTimeout(r, 50));
      ctx.waitUntil((async () => {
        await env.kv.put("nested", hits);
        throw new Error("nested waitUntil work failed");
      })());
      await new Promise((r) => setTimeout(r, 200));
    })());
    return new Response("answered first");
  },
  // Nothing of the build's pre-initialization may be shared by instances.
  "GET /fresh": () => Response.json({ random: Math.random(), uuid: crypto.randomUUID(), now: Date.now() }),
  "GET /web": async () => {
    const h = new Headers([["A", "1"], ["a", "2"], ["b", "3"]]);
    const bytes = new TextEncoder().encode("héllo");
    return Response.json({
      headers: [...h],
      text: new TextDecoder().decode(bytes),
      len: bytes.length,
      uuid: /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(crypto.randomUUID()),
      b64: btoa("hi"),
      clone: structuredClone({ a: [1, { b: 2 }] }),
      form: String(new URLSearchParams({ q: "a b", x: "&" })),
      url: (() => {
        const u = new URL("../b/c?x=1&y=2#h", "https://user:pw@example.com:8443/a/z");
        u.searchParams.append("z", "3");
        u.searchParams.delete("x");
        const parts = [u.href, u.origin, u.host, u.pathname, u.search, u.hash, u.username, u.port];
        u.pathname = "/q r";
        u.hash = "";
        u.port = "443";
        return { parts, after: u.href, all: new URL("http://h/?a=1&a=2").searchParams.getAll("a"), bad: URL.canParse("nope") };
      })(),
      decoded: new TextDecoder().decode(new Uint8Array([0xef, 0xbb, 0xbf, 0x68, 0xff])),
      fatal: (() => { try { new TextDecoder("utf-8", { fatal: true }).decode(new Uint8Array([0xff])); } catch (e) { return e.name; } })(),
      cloned: (() => { const m = structuredClone(new Map([["d", new Date(5)]])); return [m instanceof Map, m.get("d").getTime()]; })(),
      atob: atob("aGk="),
    });
  },
  "POST /put-bad": async (req, env) => {
    try { await env.kv.put("f", () => 1); } catch (e) { return Response.json({ error: e.name }); }
    return Response.json({ error: null });
  },
};

export default {
  async fetch(request, env, ctx) {
    hits++;
    const route = routes[`${request.method} ${new URL(request.url).pathname}`];
    return route ? route(request, env, ctx) : new Response("not found", { status: 404 });
  },
  // A chat room: every socket of the object joins channel "room".
  webSocketOpen(ws) {
    if (ws.id === "refuse-me") return false;
    ws.subscribe("room");
  },
  async webSocketMessage(ws, message, env) {
    if (typeof message !== "string") return ws.send(message);
    if (message.startsWith("pub ")) return env.publish("room", message.slice(4));
    if (message === "bye") return ws.close(4000);
    ws.send(`echo ${message} from ${ws.id}`);
  },
  async webSocketClose(ws, code, env) {
    await env.kv.put("closed", code);
  },
};
