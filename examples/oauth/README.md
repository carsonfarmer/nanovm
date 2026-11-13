# Sign in with GitHub

OAuth 2.0 sign-in and sessions, in about 170 lines of Rust
([src/lib.rs](src/lib.rs)) on [nanovm-sdk](../../sdk/rust).

```sh
# a GitHub OAuth app with callback http://127.0.0.1:8080/auth/callback:
# its client id in nanovm.json's config, its secret in a file of your own
echo '{"client_secret": "…"}' > ~/oauth-secrets.json
tools/build-app.sh examples/oauth -o examples/target/oauth.wasm --run --manifest examples/oauth/nanovm.json --secrets ~/oauth-secrets.json
```

Then open <http://127.0.0.1:8080/auth/login>: GitHub asks you to sign in and
sends you back to `/u-{you}/me`.

What it shows:

- **The code flow over egress.** `auth` sends the browser to GitHub with a
  random `state`, kept for ten minutes and usable once (so a link can't be
  replayed or forged); on the way back it trades the code for a token
  (`fetch` to `github.com`) and the token for the user's login
  (`api.github.com`), both in the manifest's `egress` list.
- **A sign-in finishes in the browser that started it.** `/auth/login`
  also sets a random nonce in a cookie (`HttpOnly`, `SameSite=Lax`,
  `Path=/`; `__Host-oauth_nonce` and `Secure` when `redirect_uri` is
  https, `oauth_nonce` on plain http for local runs), and the state is
  stored with the nonce's SHA-256. The callback needs the state *and* that
  nonce: otherwise anyone could start a sign-in and get someone else's
  browser to open the callback link, signing them in as the starter (login
  CSRF). A browser keeps its nonce across the sign-ins it starts, so two
  tabs both work.
- **A session cookie per user object.** The cookie (`HttpOnly`,
  `SameSite=Lax`, `Secure` over https) names its user, `session={login}.{id}`, and only that
  user's object (`/u-{login}/`) accepts it.
- **Who vouches for whom.** A call between objects can't prove who sent it
  (W-602), so a user object can't take new sessions from a call: anyone
  could make one up. Instead `auth` keeps the sessions, and a user object
  shown a cookie it hasn't seen asks `auth` (`/verify`), then remembers the
  answer. Knowing a session's 256-bit id is what proves it.
- **Sessions are durable.** They survive eviction and restarts; logout ends
  them in the user's object and in `auth`.

The client secret is not in the package: the manifest declares it
(`"secrets": ["client_secret"]`), a node's operator gives it in the node's
secrets directory, and the app reads it with its config
([Secrets](../../README.md#secrets)). The tests run this app on an
in-process dev node, and as an operator would (published without the
secret, which a supervised node gives its jailed worker), with a GitHub
stand-in on loopback.

API: `GET /auth/login` → `302` with the nonce cookie; `GET
/auth/callback?code&state` (with that cookie) → `302` with `set-cookie`,
`400` for a used, expired or unknown state or one another browser started,
`401` if GitHub refuses the code; `GET /auth/verify?session=` → the session or `404`; `GET
/u-{login}/me` → `{"login","id","name"}` or `401`; `POST /u-{login}/logout` →
`204`.

Tests: `oauth_signs_in_with_one_time_state_and_scoped_sessions`,
`review_oauth_state_must_be_bound_to_the_starting_browser` and
`oauth_and_gateway_get_their_keys_from_the_node` in
[crates/nanovm/tests/examples.rs](../../crates/nanovm/tests/examples.rs).
