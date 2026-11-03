// A durable object in JavaScript: a counter in the object's own storage.
// GET reads it; POST adds one. Each object (URL /{id}/…) has its own.
export default {
  async fetch(request, env) {
    let n = (await env.kv.get("n")) ?? 0;
    if (request.method === "POST") await env.kv.put("n", ++n);
    return Response.json({ object: env.id, n });
  },
};
