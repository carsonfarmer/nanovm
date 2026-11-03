// Workers-style Web APIs for nanovm apps, on the runtime's host functions
// (lib.rs). Evaluated once per instance before the app's module; returns
// `bind(app)`, which returns the function the runtime calls per request.
(host) => {
  const S = Symbol('state');
  const enc = new TextEncoder();
  const dec = new TextDecoder();
  const make = (C, state) => { const o = Object.create(C.prototype); o[S] = state; return o; };

  // ---- URLSearchParams (URL's own `searchParams` is a snapshot of `search`)
  const decode = (s) => { try { return decodeURIComponent(s.replace(/\+/g, ' ')); } catch { return s; } };
  const encode = (s) => encodeURIComponent(s).replace(/%20/g, '+');
  class URLSearchParams {
    constructor(init = '') {
      this[S] = [];
      if (typeof init === 'string') {
        for (const part of init.replace(/^\?/, '').split('&')) {
          if (!part) continue;
          const i = part.indexOf('=');
          this[S].push(i < 0 ? [decode(part), ''] : [decode(part.slice(0, i)), decode(part.slice(i + 1))]);
        }
      } else {
        const pairs = typeof init[Symbol.iterator] === 'function' ? init : Object.entries(init);
        for (const [k, v] of pairs) this.append(k, v);
      }
    }
    get size() { return this[S].length; }
    append(k, v) { this[S].push([String(k), String(v)]); }
    delete(k) { this[S] = this[S].filter(([n]) => n !== k); }
    get(k) { return this[S].find(([n]) => n === k)?.[1] ?? null; }
    getAll(k) { return this[S].filter(([n]) => n === k).map(([, v]) => v); }
    has(k) { return this[S].some(([n]) => n === k); }
    set(k, v) {
      const i = this[S].findIndex(([n]) => n === k);
      if (i < 0) return this.append(k, v);
      this[S] = this[S].filter(([n], j) => n !== k || j === i);
      this[S][i][1] = String(v);
    }
    forEach(f, self) { for (const [k, v] of this[S]) f.call(self, v, k, this); }
    entries() { return this[S].map(([k, v]) => [k, v])[Symbol.iterator](); }
    keys() { return this[S].map(([k]) => k)[Symbol.iterator](); }
    values() { return this[S].map(([, v]) => v)[Symbol.iterator](); }
    [Symbol.iterator]() { return this.entries(); }
    toString() { return this[S].map(([k, v]) => `${encode(k)}=${encode(v)}`).join('&'); }
  }
  Object.defineProperty(URL.prototype, 'searchParams', {
    get() { return new URLSearchParams(this.search); },
    configurable: true,
  });

  // ---- Headers: lower-case name → values, in insertion order
  class Headers {
    constructor(init) {
      this[S] = new Map();
      if (init == null) return;
      if (init instanceof Headers) {
        for (const [k, vs] of init[S]) this[S].set(k, [...vs]);
        return;
      }
      const pairs = typeof init[Symbol.iterator] === 'function' ? init : Object.entries(init);
      for (const [k, v] of pairs) this.append(k, v);
    }
    append(k, v) {
      k = String(k).toLowerCase();
      const vs = this[S].get(k);
      vs ? vs.push(String(v)) : this[S].set(k, [String(v)]);
    }
    set(k, v) { this[S].set(String(k).toLowerCase(), [String(v)]); }
    get(k) { return this[S].get(String(k).toLowerCase())?.join(', ') ?? null; }
    getSetCookie() { return [...(this[S].get('set-cookie') ?? [])]; }
    has(k) { return this[S].has(String(k).toLowerCase()); }
    delete(k) { this[S].delete(String(k).toLowerCase()); }
    forEach(f, self) { for (const [k, v] of this) f.call(self, v, k, this); }
    *entries() { for (const k of [...this[S].keys()].sort()) yield [k, this.get(k)]; }
    *keys() { for (const [k] of this) yield k; }
    *values() { for (const [, v] of this) yield v; }
    [Symbol.iterator]() { return this.entries(); }
  }
  const flat = (h) => { const out = []; for (const [k, vs] of h[S]) for (const v of vs) out.push(k, v); return out; };
  const unflat = (a) => { const h = new Headers(); for (let i = 0; i < a.length; i += 2) h.append(a[i], a[i + 1]); return h; };

  // ---- bodies are whole: a Uint8Array or null
  const toBytes = (b) =>
    b instanceof Uint8Array ? b
    : ArrayBuffer.isView(b) ? new Uint8Array(b.buffer, b.byteOffset, b.byteLength)
    : new Uint8Array(b);
  const binary = (b) => b instanceof ArrayBuffer || ArrayBuffer.isView(b);
  const withBody = (body, headers) => {
    const h = new Headers(headers);
    let type = null;
    if (body == null) body = null;
    else if (binary(body)) body = toBytes(body).slice();
    else if (body instanceof URLSearchParams) [body, type] = [enc.encode(String(body)), 'application/x-www-form-urlencoded;charset=UTF-8'];
    else [body, type] = [enc.encode(String(body)), 'text/plain;charset=UTF-8'];
    if (type && !h.has('content-type')) h.set('content-type', type);
    return { headers: h, body, used: false };
  };
  class Body {
    get bodyUsed() { return this[S].used; }
    async bytes() {
      const s = this[S];
      if (s.used) throw new TypeError('the body was already read');
      s.used = s.body !== null;
      return s.body ?? new Uint8Array(0);
    }
    async arrayBuffer() { return (await this.bytes()).slice().buffer; }
    async text() { return dec.decode(await this.bytes()); }
    async json() { return JSON.parse(await this.text()); }
  }
  const unread = (s) => { if (s.used) throw new TypeError('the body was already read'); return s; };

  const METHODS = /^(delete|get|head|options|post|put)$/i;
  class Request extends Body {
    constructor(input, init = {}) {
      super();
      const src = input instanceof Request ? unread(input[S]) : null;
      const s = withBody(init.body, init.headers ?? (src && input.headers));
      if (init.body === undefined && src) s.body = src.body;
      const m = String(init.method ?? src?.method ?? 'GET');
      s.method = METHODS.test(m) ? m.toUpperCase() : m;
      s.url = src ? src.url : new URL(String(input)).href;
      if (s.body && (s.method === 'GET' || s.method === 'HEAD')) throw new TypeError(`a ${s.method} request has no body`);
      this[S] = s;
    }
    get method() { return this[S].method; }
    get url() { return this[S].url; }
    // An incoming request's headers stay a flat array until asked for.
    get headers() { const s = this[S]; return Array.isArray(s.headers) ? (s.headers = unflat(s.headers)) : s.headers; }
    clone() { const s = unread(this[S]); return make(Request, { ...s, headers: new Headers(this.headers) }); }
  }

  class Response extends Body {
    constructor(body = null, init = {}) {
      super();
      const s = withBody(body, init.headers);
      s.status = init.status ?? 200;
      if (!(s.status >= 200 && s.status <= 599)) throw new RangeError(`status ${s.status} is not in 200-599`);
      s.statusText = String(init.statusText ?? '');
      s.url = '';
      this[S] = s;
    }
    static json(data, init = {}) {
      const headers = new Headers(init.headers);
      if (!headers.has('content-type')) headers.set('content-type', 'application/json');
      return new Response(JSON.stringify(data), { ...init, headers });
    }
    static redirect(url, status = 302) {
      return new Response(null, { status, headers: { location: new URL(String(url)).href } });
    }
    get status() { return this[S].status; }
    get ok() { return this[S].status >= 200 && this[S].status < 300; }
    get statusText() { return this[S].statusText; }
    get headers() { return this[S].headers; }
    get url() { return this[S].url; }
    get redirected() { return false; }
    get type() { return 'default'; }
    clone() { const s = unread(this[S]); return make(Response, { ...s, headers: new Headers(s.headers) }); }
  }

  // Outgoing requests go through the host: under the app's egress policy,
  // and to `http://nanovm/…` for calls between objects and publishes.
  async function fetch(input, init) {
    const req = new Request(input, init);
    const s = req[S];
    const url = new URL(s.url);
    const [status, headers, body] = await host.send(
      s.method, url.protocol.slice(0, -1), url.host, url.pathname + url.search, flat(req.headers), s.body);
    return make(Response, { status, statusText: '', headers: unflat(headers), body, used: false, url: s.url });
  }

  const crypto = {
    getRandomValues(a) {
      if (a.byteLength > 65536) throw new RangeError('getRandomValues: at most 65536 bytes');
      new Uint8Array(a.buffer, a.byteOffset, a.byteLength).set(host.random(a.byteLength));
      return a;
    },
    randomUUID() {
      const b = host.random(16);
      b[6] = (b[6] & 0x0f) | 0x40;
      b[8] = (b[8] & 0x3f) | 0x80;
      const h = [...b].map((x) => x.toString(16).padStart(2, '0')).join('');
      return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
    },
  };

  Object.assign(globalThis, { URLSearchParams, Headers, Request, Response, fetch, crypto });

  // ---- env: the object's storage, its app's other objects, and config
  const parse = (b) => (b === undefined ? undefined : JSON.parse(dec.decode(b)));
  const kv = {
    async get(key) { return parse(host.kvGet(String(key))); },
    async put(key, value) {
      if (value === undefined) throw new TypeError('kv.put: the value is undefined');
      host.kvSet(String(key), enc.encode(JSON.stringify(value)));
    },
    async delete(key) { host.kvDelete(String(key)); },
    async list({ prefix = '' } = {}) {
      const keys = [];
      let cursor;
      do {
        const [page, next] = host.kvList(cursor);
        keys.push(...page.filter((k) => k.startsWith(prefix)));
        cursor = next;
      } while (cursor !== undefined);
      return new Map(keys.sort().map((k) => [k, parse(host.kvGet(k))]));
    },
  };
  const objects = {
    get(id) {
      const base = `http://nanovm/objects/${encodeURIComponent(id)}`;
      return {
        id: String(id),
        fetch(path, init) {
          path = String(path);
          if (!path.startsWith('/')) { const u = new URL(path); path = u.pathname + u.search; }
          return fetch(base + path, init);
        },
      };
    },
  };
  async function publish(channel, message) {
    const items = [{ channel: String(channel), formats: { 'ws-message': { content: String(message) } } }];
    const r = await fetch('http://nanovm/publish/', { method: 'POST', body: JSON.stringify({ items }) });
    if (!r.ok) throw new Error(`publish: ${r.status} ${await r.text()}`);
  }
  let config;
  const env = {
    id: '',
    kv,
    objects,
    publish,
    get config() {
      if (!config) {
        const a = host.configAll();
        config = {};
        for (let i = 0; i < a.length; i += 2) config[a[i]] = a[i + 1];
      }
      return config;
    },
  };

  // ---- WebSockets: the proxy holds the client's socket and sends its
  // events as requests (Pushpin's WebSocket-over-HTTP, with GRIP). An app
  // with webSocketMessage gets them as calls; one without gets the request.
  const EVENTS = 'application/websocket-events';
  const parseEvents = (b) => {
    const out = [];
    for (let i = 0; i < b.length;) {
      let nl = i;
      while (nl < b.length && !(b[nl] === 13 && b[nl + 1] === 10)) nl++;
      const [type, len] = dec.decode(b.subarray(i, nl)).split(' ');
      i = nl + 2;
      const n = len === undefined ? -1 : parseInt(len, 16);
      out.push([type, n < 0 ? null : b.slice(i, i + n)]);
      if (n >= 0) i += n + 2;
    }
    return out;
  };
  const event = (type, data) => {
    if (data == null) return enc.encode(`${type}\r\n`);
    const head = enc.encode(`${type} ${data.length.toString(16)}\r\n`);
    const e = new Uint8Array(head.length + data.length + 2);
    e.set(head);
    e.set(data, head.length);
    e.set([13, 10], head.length + data.length);
    return e;
  };
  // One client's socket, while an event of it is served.
  class WebSocket {
    constructor(id) { this.id = id; this[S] = []; }
    send(m) { this[S].push(typeof m === 'string' ? ['TEXT', enc.encode(`m:${m}`)] : ['BINARY', toBytes(m)]); }
    close(code = 1000) { this[S].push(['CLOSE', new Uint8Array([code >> 8, code & 255])]); }
    subscribe(channel) { this.#control('subscribe', channel); }
    unsubscribe(channel) { this.#control('unsubscribe', channel); }
    #control(type, channel) { this[S].push(['TEXT', enc.encode(`c:${JSON.stringify({ type, channel: String(channel) })}`)]); }
  }
  async function sockets(app, request, env, ctx) {
    const ws = new WebSocket(request.headers.get('connection-id'));
    for (const [type, data] of parseEvents(await request.bytes())) {
      if (type === 'OPEN') {
        if ((await app.webSocketOpen?.(ws, env, ctx)) === false) return new Response('refused', { status: 403 });
        ws[S].unshift(['OPEN', null]);
      } else if (type === 'TEXT') await app.webSocketMessage(ws, dec.decode(data), env, ctx);
      else if (type === 'BINARY') await app.webSocketMessage(ws, data, env, ctx);
      else if (type === 'CLOSE') await app.webSocketClose?.(ws, data?.length === 2 ? (data[0] << 8) | data[1] : 1005, env, ctx);
      else if (type === 'DISCONNECT') await app.webSocketClose?.(ws, 1006, env, ctx);
    }
    const parts = ws[S].map(([t, d]) => event(t, d));
    const body = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
    parts.reduce((at, p) => (body.set(p, at), at + p.length), 0);
    return new Response(body, { headers: { 'content-type': EVENTS, 'sec-websocket-extensions': 'grip' } });
  }

  class ExecutionContext {
    constructor() { this[S] = []; }
    waitUntil(p) { this[S].push(p); }
    passThroughOnException() {}
  }

  return (app) => {
    if (typeof app?.fetch !== 'function') throw new TypeError('the module must `export default { fetch(request, env, ctx) }`');
    return async (id, method, url, headers, body) => {
      env.id = id;
      const request = make(Request, { method, url, headers, body, used: false });
      const ctx = new ExecutionContext();
      const ws = typeof app.webSocketMessage === 'function' && method === 'POST' && request.headers.get('content-type') === EVENTS;
      const res = await (ws ? sockets(app, request, env, ctx) : app.fetch(request, env, ctx));
      if (!(res instanceof Response)) throw new TypeError('fetch() must return a Response');
      const s = unread(res[S]);
      return [s.status, flat(s.headers), s.body, ctx[S].length ? Promise.all(ctx[S]) : undefined];
    };
  };
}
