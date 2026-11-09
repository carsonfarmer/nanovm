# Feature flags

Feature flags with per-tenant overrides, live updates and percentage
rollouts, in about 80 lines of Rust ([src/lib.rs](src/lib.rs)) on
[nanovm-sdk](../../sdk/rust).

```sh
tools/build-app.sh examples/flags -o examples/target/flags.wasm --run --manifest examples/flags/nanovm.json
```

```sh
curl http://127.0.0.1:8080/acme/flags                  # {"dark_mode":{"value":true,"source":"default"},…}
curl -XPUT http://127.0.0.1:8080/acme/flags/new_checkout -d true
curl -XPUT http://127.0.0.1:8080/acme/flags/search_v2 -d '{"percent":50}'
curl 'http://127.0.0.1:8080/acme/flags?user=ann'       # each rollout decided for ann
```

A WebSocket on `/acme/ws` gets the flags, then the flags after each change.

What it shows:

- **Defaults in config, overrides in objects.** The flags and their
  defaults are the manifest's `config` (`flag.<name>`, read with
  `wasi:config`), the same for every tenant and changed by publishing a new
  version. Each tenant is an object whose key-value store holds only what it
  overrides; `DELETE` returns a flag to its default.
- **Live updates for what committed.** A change publishes the tenant's flags
  to its sockets once it has committed; other tenants hear nothing.
- **Rollouts that are stable.** `{"percent": n}` turns a flag on for a share
  of users by hashing flag and user into 100 buckets (FNV-1a, so the same
  across builds and restarts): a user gets the same answer every time, and
  raising the share only adds users.

Anyone can change a tenant's flags here; a real service puts its admin
routes behind sign-in.

API: `GET /{tenant}/flags` (`?user=…`) → `{"<name>":{"value","source"}}`;
`PUT /{tenant}/flags/{name}` (`true`, `false` or `{"percent":0-100}`) and
`DELETE` → the flag (`404` for a flag the config doesn't define, `400` for a
bad value); `/{tenant}/ws`.

Test: `flags_override_defaults_per_tenant_live` in
[crates/nanovm/tests/examples.rs](../../crates/nanovm/tests/examples.rs).
