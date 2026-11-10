# Auth: JWT verification, argon2id passwords, sessions

A Wasm component with three small interfaces an app composes with `wac plug`,
like the other components:

```sh
bash components/auth/build.sh   # target/components/auth/{provider,tests}.wasm
wac plug app.wasm --plug target/components/auth/provider.wasm -o app-with-auth.wasm
```

[`nanovm:auth@0.1.0`](wit/auth.wit):

- **`jwt`**: `verify(token, key, options)` returns the claims. Keys: an HMAC
  secret (HS256/384/512 only), an inline JWK Set, or a JWK Set URL fetched
  through the app's own egress allowlist and cached in `/data/.auth/jwks`
  for `ttl-s` (a cold instance reads the cache instead of refetching; an
  unknown `kid` refetches at most once a minute, for key rotation; a stale set
  still verifies while the issuer is down). Algorithms: RS*, PS*, ES256,
  ES384, EdDSA. Checks `exp`/`nbf` with leeway, `iss`, `aud`, and an optional
  algorithm allowlist. Refuses `alg: none`, `crit` headers, RSA keys under
  2,048 bits, and algorithm confusion: an HMAC token never verifies against a
  JWK Set, a key's `kty`/`crv` must fit the algorithm, and a key's own `alg`
  and `use` are enforced. `sign-hs256` issues an app's own tokens.
- **`passwords`**: argon2id PHC strings with a random salt. The default cost
  is OWASP's minimum (19 MiB, 2 passes). Params are bounded (at most 64 MiB,
  10 passes, 4 lanes), including in stored hashes, so a planted hash can't
  make verification take unbounded memory or time. `needs-rehash` tells an
  app to upgrade a hash after a successful login.
- **`sessions`**: server-side sessions, one file per session in
  `/data/.auth/sessions`, named by a SHA-256 of the 256-bit token, so the
  volume never holds a usable token. `create`, `get` (unknown or expired is
  `none`), `refresh`, `destroy`, `destroy-subject` (log out everywhere) and
  `sweep` (delete expired ones).

The crypto is RustCrypto's pure-Rust crates (`rsa`, `p256`, `p384`,
`ed25519-dalek`, `hmac`, `sha2`, `argon2`). Randomness comes from
`wasi:random`.

## Why it needs no write-ahead log

nanovm commits all of a request's changes to `/data` together, or rolls them
all back, so a session created by a request that then fails never existed.
Each call touches one file: a new session is created whole (removed again if
its write is refused), a refresh writes a sibling and renames it over, so an
error return means nothing changed; if that cleanup fails, the component traps
and the request rolls back. `destroy-subject` removes all or (by trapping)
none. The JWK Set cache is only an optimization: a refused cache write is
ignored.

## Limits and cost

- Sessions: one file each against `max_files` (the default 1,000 holds about
  990 per object): give each user or tenant its own object, or sweep. Subjects
  are 1-256 bytes, data at most 64 KiB, TTL 1 second to 10 years.
- JWT verification in Wasm is slower than native: per request about 1.3 ms
  for RS256, 2.2 ms for ES256 and 0.4 ms for EdDSA on this VM (an
  in-process node, dev profile with optimized dependencies). Enabling the
  `rsa` crate's 64-bit digits made RS256 slower (2.2 ms): Wasm has no 128-bit
  multiply.
- argon2id costs about 65 ms CPU at the default params (19 MiB, 2 passes) and
  12 ms at 8 MiB and 1 pass, charged to the request: keep the app's `cpu_ms`
  well above it (the default 200 ms fits) and `memory_mb` above the memory
  param plus the app's own.
- A JWK Set over 256 KiB is refused. The URL's host must be in the app's
  egress list.

More numbers and method: [notes/components.md](../../notes/components.md).

## Tests

`crates/nanovm/tests/components.rs`, `auth_*`: JWT contracts against fixed
vectors ([tests/src/vectors.json](tests/src/vectors.json), generated once
with Python `cryptography`: RSA-2048, P-256 and Ed25519 keys, tokens expiring
in 2100) including expiry, not-before, issuer, audience, tampering, unknown
`kid`, `none` and HMAC-with-public-key confusion; password hashing and bounds;
JWK Set fetching, caching across restart, rotation and outage against a local
HTTP server; sessions across eviction and restart, rollback of a failed
request, expiry, refresh, sweep and log-out-everywhere; file and byte quota
exhaustion. Dropping the cleanup of a refused session write fails
`auth_sessions_quota_exhaustion_is_refused_cleanly`.
