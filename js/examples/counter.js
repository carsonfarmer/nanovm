// A durable object in JavaScript: one counter per object, in its storage.
export default {
  async fetch(request, env) {
    const n = ((await env.kv.get("n")) ?? 0) + 1;
    await env.kv.put("n", n);
    return Response.json({ object: env.id, n });
  },
};
