// Workers-style Web APIs for nanovm apps, on the runtime's host functions
// (lib.rs). Evaluated once per instance before the app's module; returns
// `bind(app)`, which returns `[serve, answer]`, the functions the runtime
// calls per request. QuickJS brings the language, atob/btoa,
// queueMicrotask and DOMException.
(host) => {
  const S = Symbol('state');
  const make = (C, state) => { const o = Object.create(C.prototype); o[S] = state; return o; };
  const toBytes = (b) =>
    b instanceof Uint8Array ? b
    : ArrayBuffer.isView(b) ? new Uint8Array(b.buffer, b.byteOffset, b.byteLength)
    : new Uint8Array(b);

  // QuickJS seeds its own when the engine starts: at build time.
  Math.random = host.mathRandom;

  // ---- TextEncoder / TextDecoder (UTF-8)
  class TextEncoder {
    get encoding() { return 'utf-8'; }
    encode(s = '') { return host.encode(String(s).toWellFormed()); }
  }
  class TextDecoder {
    constructor(label = 'utf-8', { fatal = false, ignoreBOM = false } = {}) {
      if (!/^\s*(unicode-1-1-)?utf-?8\s*$/i.test(label)) throw new RangeError(`TextDecoder: ${label} is not supported`);
      this[S] = [!!fatal, !!ignoreBOM];
    }
    get encoding() { return 'utf-8'; }
    get fatal() { return this[S][0]; }
    get ignoreBOM() { return this[S][1]; }
    decode(b) { return b === undefined ? '' : host.decode(toBytes(b), this[S][0], this[S][1]); }
  }
  const enc = new TextEncoder();
  const dec = new TextDecoder();

  // ---- console: log, info and debug to the object's stdout; warn and error to its stderr
  const show = (v) => {
    if (typeof v === 'string') return v;
    if (v instanceof Error) return v.stack ? `${v}\n${v.stack.trimEnd()}` : String(v);
    if (typeof v === 'object' && v !== null) try { return JSON.stringify(v); } catch {}
    return String(v);
  };
  const log = (fd) => (...a) => host.print(fd, a.map(show).join(' '));
  const console = { log: log(1), info: log(1), debug: log(1), warn: log(2), error: log(2) };

  // ---- timers: the host calls back; a timer still pending when the
  // request (and its waitUntil work) is done is dropped.
  const timers = new Set();
  let timerId = 0;
  const timer = (repeat) => (f, ms = 0, ...args) => {
    const id = ++timerId;
    const delay = Math.max(0, Number(ms) || 0);
    const tick = () => {
      if (!timers.has(id)) return;
      if (repeat) host.sleep(delay, tick);
      else timers.delete(id);
      f(...args);
    };
    timers.add(id);
    host.sleep(delay, tick);
    return id;
  };
  const clearTimer = (id) => { timers.delete(id); };

  // ---- structuredClone: plain data, Date, RegExp, Map, Set, binary
  const clone = (v, seen) => {
    if (typeof v === 'function' || typeof v === 'symbol') throw new DOMException(`a ${typeof v} could not be cloned`, 'DataCloneError');
    if (typeof v !== 'object' || v === null) return v;
    if (seen.has(v)) return seen.get(v);
    const put = (c) => (seen.set(v, c), c);
    if (v instanceof Date) return put(new Date(v));
    if (v instanceof RegExp) return put(new RegExp(v.source, v.flags));
    if (v instanceof ArrayBuffer) return put(v.slice(0));
    if (ArrayBuffer.isView(v)) return put(new v.constructor(clone(v.buffer, seen), v.byteOffset, v instanceof DataView ? v.byteLength : v.length));
    if (v instanceof Map) { const m = put(new Map()); for (const [k, x] of v) m.set(clone(k, seen), clone(x, seen)); return m; }
    if (v instanceof Set) { const m = put(new Set()); for (const x of v) m.add(clone(x, seen)); return m; }
    const o = put(Array.isArray(v) ? [] : {});
    for (const k of Object.keys(v)) o[k] = clone(v[k], seen);
    return o;
  };
  const structuredClone = (v) => clone(v, new Map());

  // ---- URLSearchParams; a URL's own is live (changing either changes both)
  const decode = (s) => { try { return decodeURIComponent(s.replace(/\+/g, ' ')); } catch { return s; } };
  const encode = (s) => encodeURIComponent(s).replace(/%20/g, '+').replace(/[!'()~]/g, (c) => `%${c.charCodeAt(0).toString(16).toUpperCase()}`);
  const pairs = (s) => s.replace(/^\?/, '').split('&').filter(Boolean).map((p) => {
    const i = p.indexOf('=');
    return i < 0 ? [decode(p), ''] : [decode(p.slice(0, i)), decode(p.slice(i + 1))];
  });
  const OWNER = Symbol('url');
  class URLSearchParams {
    constructor(init = '') {
      this[S] = [];
      if (typeof init === 'string') this[S] = pairs(init);
      else for (const [k, v] of typeof init[Symbol.iterator] === 'function' ? init : Object.entries(init)) this[S].push([String(k), String(v)]);
    }
    get size() { return this[S].length; }
    append(k, v) { this[S].push([String(k), String(v)]); this.#changed(); }
    delete(k) { this[S] = this[S].filter(([n]) => n !== k); this.#changed(); }
    get(k) { return this[S].find(([n]) => n === k)?.[1] ?? null; }
    getAll(k) { return this[S].filter(([n]) => n === k).map(([, v]) => v); }
    has(k) { return this[S].some(([n]) => n === k); }
    set(k, v) {
      const i = this[S].findIndex(([n]) => n === k);
      if (i < 0) return this.append(k, v);
      this[S] = this[S].filter(([n], j) => n !== k || j === i);
      this[S][i][1] = String(v);
      this.#changed();
    }
    sort() { this[S].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)); this.#changed(); }
    forEach(f, self) { for (const [k, v] of this[S]) f.call(self, v, k, this); }
    entries() { return this[S].map(([k, v]) => [k, v])[Symbol.iterator](); }
    keys() { return this[S].map(([k]) => k)[Symbol.iterator](); }
    values() { return this[S].map(([, v]) => v)[Symbol.iterator](); }
    [Symbol.iterator]() { return this.entries(); }
    toString() { return this[S].map(([k, v]) => `${encode(k)}=${encode(v)}`).join('&'); }
    #changed() {
      const url = this[OWNER];
      if (url) url[S] = host.urlSet(url[S][0], 'search', this.toString());
    }
  }

  // ---- URL: the url crate parses (WHATWG); host.url returns the parts
  const PARTS = ['href', 'origin', 'protocol', 'username', 'password', 'host', 'hostname', 'port', 'pathname', 'search', 'hash'];
  class URL {
    constructor(url, base) { this[S] = host.url(String(url), base === undefined ? undefined : String(base)); }
    static canParse(url, base) { try { new URL(url, base); return true; } catch { return false; } }
    static parse(url, base) { try { return new URL(url, base); } catch { return null; } }
    get searchParams() {
      if (!this[OWNER]) this[OWNER] = Object.assign(new URLSearchParams(this[S][9]), { [OWNER]: this });
      return this[OWNER];
    }
    toString() { return this[S][0]; }
    toJSON() { return this[S][0]; }
  }
  PARTS.forEach((name, i) => Object.defineProperty(URL.prototype, name, {
    get() { return this[S][i]; },
    set: name === 'origin' ? undefined : function (v) {
      this[S] = host.urlSet(this[S][0], name, String(v));
      if (this[OWNER]) this[OWNER][S] = pairs(this[S][9]);
    },
    enumerable: true,
    configurable: true,
  }));

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
      if (vs) vs.push(String(v));
      else this[S].set(k, [String(v)]);
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
  let flatOut;
  const flatOne = (vs, k) => { for (let i = 0; i < vs.length; i++) flatOut.push(k, vs[i]); };
  const flat = (h) => {
    flatOut = [];
    h[S].forEach(flatOne);
    return flatOut;
  };
  const unflat = (a) => { const h = new Headers(); for (let i = 0; i < a.length; i += 2) h.append(a[i], a[i + 1]); return h; };

  // ---- bodies are whole: a Uint8Array or null
  const binary = (b) => b instanceof ArrayBuffer || ArrayBuffer.isView(b);
  // A request's or response's state: its headers, and its body as a Uint8Array or null.
  const withBody = (body, headers, type) => {
    const h = new Headers(headers);
    if (body == null) body = null;
    else if (binary(body)) body = toBytes(body).slice();
    else {
      type ??= body instanceof URLSearchParams ? 'application/x-www-form-urlencoded;charset=UTF-8' : 'text/plain;charset=UTF-8';
      body = host.encode(String(body).toWellFormed());
    }
    if (type && !h[S].has('content-type')) h[S].set('content-type', [type]);
    return { headers: h, body, used: false };
  };
  // Request's and Response's body methods (a mixin: a base class would make
  // every construction a derived one, which costs QuickJS a lot more).
  class Body {
    get bodyUsed() { return this[S].used; }
    async bytes() {
      const s = this[S];
      if (s.used) throw new TypeError('the body was already read');
      s.used = s.body !== null;
      return s.body ?? new Uint8Array(0);
    }
    async arrayBuffer() { return (await this.bytes()).slice().buffer; }
    async text() { return host.decode(await this.bytes(), false, false); }
    async json() { return JSON.parse(await this.text()); }
  }
  const unread = (s) => { if (s.used) throw new TypeError('the body was already read'); return s; };

  const METHODS = /^(delete|get|head|options|post|put)$/i;
  class Request {
    constructor(input, init = {}) {
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

  const response = (body, init, type) => {
    const s = withBody(body, init.headers, type);
    s.status = init.status ?? 200;
    if (!(s.status >= 200 && s.status <= 599)) throw new RangeError(`status ${s.status} is not in 200-599`);
    s.statusText = init.statusText === undefined ? '' : String(init.statusText);
    s.url = '';
    return s;
  };
  class Response {
    constructor(body = null, init = {}) { this[S] = response(body, init); }
    static json(data, init = {}) { return make(Response, response(JSON.stringify(data), init, 'application/json')); }
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

  const { constructor: _, ...bodyMethods } = Object.getOwnPropertyDescriptors(Body.prototype);
  for (const C of [Request, Response]) Object.defineProperties(C.prototype, bodyMethods);

  // Outgoing requests go through the host: under the app's egress policy,
  // and to `http://nanovm/…` for calls between objects and publishes.
  async function fetch(input, init) {
    const req = new Request(input, init);
    const s = req[S];
    const answer = await host.send(s.method, s.url, flat(req.headers), s.body);
    if (typeof answer === 'string') throw new TypeError(answer);
    const [status, headers, body] = answer;
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

  Object.assign(globalThis, {
    TextEncoder, TextDecoder, console, URL, URLSearchParams, Headers, Request, Response, fetch, crypto, structuredClone,
    setTimeout: timer(false), setInterval: timer(true), clearTimeout: clearTimer, clearInterval: clearTimer,
  });

  // ---- env: the object's storage, its app's other objects, and config
  // Values are stored as JSON.
  const kv = {
    async get(key) { return host.kvGet(String(key)); },
    async put(key, value) { host.kvSet(String(key), value); },
    async delete(key) { host.kvDelete(String(key)); },
    async list({ prefix = '' } = {}) {
      const keys = [];
      let cursor;
      do {
        const [page, next] = host.kvList(cursor);
        keys.push(...page.filter((k) => k.startsWith(prefix)));
        cursor = next;
      } while (cursor !== undefined);
      return new Map(keys.sort().map((k) => [k, host.kvGet(k)]));
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

  // No async functions here: the runtime waits for the app's promise itself.
  return (app) => {
    if (typeof app?.fetch !== 'function') throw new TypeError('the module must `export default { fetch(request, env, ctx) }`');
    // `serve(id, method, url, headers, body)`: `[the app's response or a promise of it, ctx]`.
    const serve = (id, method, url, headers, body) => {
      env.id = id;
      const request = make(Request, { method, url, headers, body, used: false });
      const ctx = new ExecutionContext();
      const ws = typeof app.webSocketMessage === 'function' && method === 'POST' && request.headers.get('content-type') === EVENTS;
      return [ws ? sockets(app, request, env, ctx) : app.fetch(request, env, ctx), ctx];
    };
    // `answer(response, ctx)`: `[status, headers, body, waitUntil promise or undefined]`.
    const answer = (res, ctx) => {
      if (!(res instanceof Response)) throw new TypeError('fetch() must return a Response');
      const s = unread(res[S]);
      return [s.status, flat(s.headers), s.body, ctx[S].length ? Promise.all(ctx[S]) : undefined];
    };
    return [serve, answer];
  };
}
