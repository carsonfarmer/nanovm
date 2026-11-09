# Sign in with GitHub

OAuth 2.0 sign-in and sessions, in about 140 lines of Rust
([src/lib.rs](src/lib.rs)) on [nanovm-sdk](../../sdk/rust).

```sh
# a GitHub OAuth app with callback http://127.0.0.1:8080/auth/callback;
# its client id in nanovm.json's config, its secret as env CLIENT_SECRET
tools/build-app.sh examples/oauth -o examples/target/oauth.wasm --run --manifest examples/oauth/nanovm.json
```

Then open <http://127.0.0.1:8080/auth/login>: GitHub asks you to sign in and
sends you back to `/u-{you}/me`.

What it shows:

- **The code flow over egress.** `auth` sends the browser to GitHub with a
  random `state`, kept for ten minutes and usable once (so a link can't be
  replayed or forged); on the way back it trades the code for a token
  (`fetch` to `github.com`) and the token for the user's login
  (`api.github.com`), both in the manifest's `egress` list.
- **A session cookie per user object.** The cookie (`HttpOnly`,
  `SameSite=Lax`) names its user, `session={login}.{id}`, and only that
  user's object (`/u-{login}/`) accepts it.
- **Who vouches for whom.** A call between objects can't prove who sent it
  (W-602), so a user object can't take new sessions from a call: anyone
  could make one up. Instead `auth` keeps the sessions, and a user object
  shown a cookie it hasn't seen asks `auth` (`/verify`), then remembers the
  answer. Knowing a session's 256-bit id is what proves it.
- **Sessions are durable.** They survive eviction and restarts; logout ends
  them in the user's object and in `auth`.

`CLIENT_SECRET` is in the manifest's `env`, which is published with the app:
real secrets need a secret store (IDEAS G-78). The test runs this app on an
in-process dev node with a GitHub stand-in on loopback.

API: `GET /auth/login` → `302`; `GET /auth/callback?code&state` → `302` with
`set-cookie`, `400` for a used, expired or unknown state, `401` if GitHub
refuses the code; `GET /auth/verify?session=` → the session or `404`; `GET
/u-{login}/me` → `{"login","id","name"}` or `401`; `POST /u-{login}/logout` →
`204`.

Test: `oauth_signs_in_with_one_time_state_and_scoped_sessions` in
[crates/nanovm/tests/examples.rs](../../crates/nanovm/tests/examples.rs).
