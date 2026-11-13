# Beta: app secrets (B-100)

The production audit found no secrets mechanism: an app's only inputs were its manifest's `env`
and `config`, which are the OCI config blob of a package that nodes pull anonymously, so is
public. This note records what was built on `beta/secrets`, why, and the evidence.

## What an app and an operator do

- The manifest declares the secrets the app needs, by `wasi:config` key: `"secrets": ["client_secret"]`.
- The node's config names a directory: `secrets = "/etc/nanovm/secrets"`. Only the user nanovm runs as
  may use it (owner, no group or other bits), or the supervisor refuses to start.
- It holds each app's secrets as `<app>.json`, a JSON object of strings. `<app>` is the app's name on
  the node, `registry::app_name(host, reference)`, the same key as its volumes and uid.
- The guest reads them with its config: `config::get("client_secret")` (Rust),
  `env.config.client_secret` (JavaScript). A secret overrides a package config value of the same name.
- `nanovm run app.wasm --secrets secrets.json` does the same locally.

## Choices and why

**`wasi:config`, not environment variables.** Apps already read settings through
`wasi:config/store` (`config::get` in six examples). The JavaScript runtime exposes only config
(`env.config`, built from `get_all`), not the WASI environment, so env would leave JavaScript apps
without secrets. Environment variables are also copied into every instance's `WasiCtx`, and
programs tend to print them (debug dumps, crash reporters). Spin's variables, which may be marked
`secret`, are served through the same `wasi:config` interface.

**A root-only directory, one file per app, read when the worker starts.** The starting point from the
brief, kept: it needs no new service, the supervisor (root) already reads the app's other inputs at
start, and the worker spec channel already exists. Files are compatible with how operators already
hold secrets: a systemd `LoadCredential=` directory, a Kubernetes secret volume, or files that
`sops`/`age` decrypt at boot all fit, as long as the directory's mode is 0700 or tighter. Keyed by the
app's name (host + registry/repository), so a secret follows exactly what volumes follow: another host
pointing its DNS at the same repository is another app and gets none of them.

**The spec channel.** The supervisor sends the values in the worker's spec line on its stdin, a pipe
only the two processes hold. The worker is spawned with `env_clear()` and the single argument
`worker`. `Secrets` has a `Debug` that prints only keys, since `AppConfig` derives `Debug`.

**What a change does: it restarts the app's worker.** A rotation after a leak must stop the old value
being used, and "next worker start" can mean never for a busy app. The supervisor checks each
running app's file every second (one `stat`: inode, size, mtime). A change retires the worker the way
a version switch does (requests in flight finish; the slot is freed), and the next request starts
one, reading the file afresh. Removing the file also restarts the app without secrets, or fails it if
it declares any. `touch` forces a restart. Cost: one `stat` per running app per second, off the
request path. `nanovm run` restarts the same way when the `--secrets` file changes.

**Fail closed.** A declared secret the file doesn't give, a file that isn't a regular file (a FIFO
would block the read), can't be read, or isn't a JSON object of strings stops the app: the start
fails, its requests get 502 with the reason, and the supervisor retries after its usual 1 s backoff.
serde_json's own errors can quote what they read (`invalid type: integer `4242…``, `invalid type:
string "sk-…"`), so the parse error is rebuilt from its line and column only. Errors name the file
(`<app>.json`), not the directory, since clients see them (B-105).

**Masking what the host reports.** The host never copies a secret into telemetry or a client's
error by itself, but it does pass on what the guest wrote: its stdout and stderr (log records, and
the tail in a trapped request's error detail), the paths of its requests out (`url.path` on client
spans; a Telegram bot token or Slack webhook lives in the path), and a returned error's text. The
worker masks every secret value in each telemetry batch before it leaves (in `Client::send_bytes`,
so every span, event and log is covered whatever its field), and in the two error details built from
guest text. Masking is byte for byte with `*`: protobuf lengths stay right, so a batch still parses,
and whole UTF-8 characters are replaced, so text stays UTF-8. It matches exact values (B-101).

**`nanovm publish` refuses, rather than warns.** A pushed package is public and cached; a warning in
CI output is easy to miss. It refuses:
- a key listed in `secrets` that also appears in `config` or `env` (as Spin refuses a default for a
  secret variable);
- a non-empty value under a key whose letters end in `secret`, `password`, `passwd`, `passphrase`,
  `token`, `credential(s)`, `apikey`, `privatekey`, `accesskey` or `secretkey` (so `client_secret`,
  `BOT_TOKEN`, `dbPassword`; not `token_url`, `max_tokens`, `password_min_length`);
- a value shaped like a credential: a PEM private key, a URL with a password, or a token with a
  well-known prefix and at least 16 more characters (`sk-`, `sk_live_`, `rk_live_`, `ghp_`, `gho_`,
  `ghu_`, `ghs_`, `ghr_`, `github_pat_`, `glpat-`, `xoxb-`, `xoxp-`, `AKIA`, `AIza`, `hf_`).
The message names the table and key, never the value. All 23 example manifests pass
(`example_manifests_hold_no_secrets`). Libraries: secret scanners (gitleaks' rules, `ripsecrets`,
Nosey Parker) cover hundreds of formats but are tools, not small libraries; a dependency for this
short list wasn't worth it.

## Evidence

All runs as root on the shared 4-core machine, debug builds, `RUST_TEST_THREADS=1`.

- **Isolation** (`secrets_reach_only_their_apps_worker`, jailed workers): app A reads its key through
  `wasi:config` (200); app B, the same component from another repository, gets 404 for the same key;
  once B is given its own, B's worker restarts with it and A still reads A's. A's worker runs as uid
  200,000+; its `/proc/<pid>/cmdline` and `environ` don't hold the key; `cat` of A's secrets file run
  as the worker's uid fails; the worker's `/proc/<pid>/environ` and `mem` are owned by root (it is not
  dumpable, so no core dump or same-uid process reads its memory).
- **Change and failure** (`changed_or_bad_secrets_restart_or_stop_the_app`): with no file, 502
  `the app's secrets, from the node's secrets directory: the app needs secrets api_key; <app>.json
  doesn't give them`; given, 200 `one`; rewritten, 200 `two` from a new worker pid; a file holding
  `{"api_key": 4242424242424242}` gives 502 `<app>.json is not a JSON object of strings (line 1,
  column …)` without `4242`; a directory in its place, 502 `<app>.json is not a file`; fixed, 200
  again; a 0755 directory stops `Supervisor::new` with `only its owner, the user nanovm runs as, may
  use it (chmod 700)`.
- **Reporting** (`secrets_stay_out_of_telemetry_and_error_text`, a private test component's `/leak`
  route): the guest prints its key to stdout and stderr, requests `http://localhost:9/leak/<key>`
  (refused by the egress filter, but its span records the path), then panics with it, or returns it
  in an `InternalError`. The client's 500 bodies hold `panic ****…`, `stderr ****…` and `error ****…`
  and not the key. Every byte the collector receives holds `/leak/****…`, `stdout ****…`,
  `stderr ****…` and `panic ****…` and not the key.
- **Examples end to end** (`oauth_and_gateway_get_their_keys_from_the_node`): each example is pushed
  to a loopback registry with its manifest (which `check_package` accepts and which holds no key),
  served by a supervisor in dev mode with jailed workers, its secrets file named by
  `app_name("<name>.example", reference)`. oauth signs `ann` in against the GitHub stand-in, which
  accepts only `client_secret = s3cret`; the gateway streams a completion, and the API stand-in
  recorded exactly `Bearer test-key`. The in-process example tests take the same keys as `secrets`.
- **Local** (`nanovm_run_takes_secrets_from_a_file`): without `--secrets` the app doesn't start
  (`--secrets <secrets.json>: the app needs secrets api_key; none were given`); a malformed file is
  refused without its value; `one` is served, the file is rewritten, `nanovm run` says
  `secrets.json changed; restarting` and serves `two`.
- **Publish** (`nanovm_publish_pushes_what_a_node_loads`): three leaky manifests (a declared secret
  in `config`, `env.CLIENT_SECRET`, an `sk-proj-…` value) are refused, no output holds the value, and
  nothing was pushed under that tag.
- **Unit** (`secrets.rs`): loading, refusal messages that never quote the file (four malformed
  shapes), masking of overlapping values in place, the directory check, and the publish rules.

### Mutation checks

Each mechanism broken in turn on the committed tree, its tests run, then restored with
`git checkout -- <file>` and `git diff --quiet -- crates` checked:

| Mutation | Fails |
|---|---|
| parse errors carry serde's message (`{e}`) | `load_reads_a_file_and_refuses_without_quoting_it`: the error quotes `"sk-0123456789abcdefSECRET"` |
| a needed secret isn't required | the same: `load` returns `Ok({})` |
| the directory's mode isn't checked | `the_directory_is_private`: 0750 accepted |
| a declared secret allowed in `config` | `packages_refuse_credentials` |
| credential-like keys allowed | `packages_refuse_credentials` |
| credential-like values allowed | `packages_refuse_credentials` |
| `Debug` shows values | `load_reads_…`: `{("api_key", "k-123"), …}` |
| telemetry batches not masked | `secrets_stay_out_of_telemetry_and_error_text`: `/leak/<key>` reported as is |
| a trap's detail not masked | the same: the client's 500 holds `stdout sk-leak-…`, `panic sk-leak-…` |
| a guest error's detail not masked | the same: `ErrorCode::InternalError(Some("error sk-leak-…"))` |
| secrets not added to `wasi:config` | `secrets_reach_only_their_apps_worker`: `(404, "no such config key")` |
| no restart when the file changes | `changed_or_bad_secrets_restart_or_stop_the_app`: still `200 one` after 10 s |
| the directory not checked at start | the same: the supervisor starts on a 0755 directory |
| `nanovm run` doesn't watch `--secrets` | `nanovm_run_takes_secrets_from_a_file`: no restart line in 60 s |

All 14 restored cleanly. The script: `scratchpad/b-secrets/mutate.py` (not committed).

### Cost

- Runtime lines (CI's count): 7,410 → 7,542 (+132): `secrets.rs` 80, `supervisor.rs` 32 (read at
  start, the watcher, `retire`), `telemetry.rs` 8, `config.rs` 5, `main.rs` 4, `registry.rs` 2,
  `lib.rs` 1. One new direct dependency, `memchr` (already in the tree through other crates).
- The request path is unchanged: config is built once per worker; masking runs on error details and
  on telemetry batches, on the export thread. Copying and masking a full 1 MiB batch (the most one
  may be, hundreds of requests' worth) took a median 84 µs with one secret and 131 µs with three
  (50 runs each; a release build of the same function in a scratch program; the machine was
  under other agents' load, worst run 4 ms). Apps without secrets skip it.
- The watcher: one `stat` per running app per second.

## Not done, or not verified

- Developers setting their own secrets (B-103): needs authenticated deploys.
- Private registry pulls with credentials (B-102): recorded, not built. Packages no longer hold
  secrets, so public packages leak only code; needed only if code must stay private.
- Several nodes: each reads its own directory; nothing copies secrets between them.
- Masking is exact-match only (B-101); a non-root worker stays dumpable (B-104).
- At rest the file is plain text, protected by the directory's mode; encryption at rest is the
  operator's (an encrypted disk, or decrypting into a tmpfs at boot).
- Not measured on a release node under load; the request path is unchanged, so no CPU-per-request
  comparison was run.

## Owner questions

1. **Should developers set their own secrets later?** Recommended: yes, after authenticated deploys
   land: a deploy authenticated as the identity the app's trust policy names may set that app's
   secrets, sealed to the node's key. Until then, operators set them (this branch).
2. **Private registries?** Recommended: not for the beta (B-102).
3. **Is refusing (not warning) at publish right?** Recommended: yes; a pushed secret can't be
   unpublished. The rules are narrow; a false positive is fixed by declaring the key a secret or
   renaming it.
