# Secrets audit: every long-lived secret nanovm expects someone to hold

The owner wants no long-lived secrets, tokens, keys or credentials, wherever that's possible. This
audit lists every one nanovm expects the node, the proxy, the operator, the developer, an app or CI
to hold or provide, and every place where trust rests on something other than a credential. For
each it gives the short-lived or secretless option that exists today, and whether that option
needs new nanovm code.

Read-only: no code was changed and nothing was built or run.

- **Refs.** `M` is `origin/main` at `0f9ee0f` (round 3). `B` is `origin/beta/integration` at
  `7988cae` (PR #3: the secrets directory, the admin socket, the `hosts`/`signers` allowlists, the
  deploy material). `M+B` means both. Paths are `ref:path:line`.
- **Method.** `git grep` over both refs for secret, token, password, credential, key, HMAC,
  signing, `AWS_`, `Authorization`, `LoadCredential`, `EnvironmentFile` and similar, then each hit
  read in context. The vendored `object_store` 0.14.2 source was read for the credential chain its
  `aws-base` feature provides. Code, configs, systemd units, Caddy, the container image, docs,
  runbooks, CI workflows, the SDK, the JS runtime, the examples, scripts and tests were covered.
- **Links** point at vendor docs. They could not be fetched from the audit sandbox (its proxy
  allowed only docs.rs), so check them before relying on them.

## Summary, by risk

Risk is the blast radius if the secret is stolen, times how likely it is to be stolen where it
lives. "Absence" rows hold no secret: trust rests on network position, and the fix should stay
secretless.

| # | Risk | Ref | What | Where | Held by · lifetime | If stolen | Short-lived or secretless option today | New nanovm code? |
|---|---|---|---|---|---|---|---|---|
| S1 | **Critical** | M+B | Object-store credential for leases **and** the capture store (static `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`) | `M+B:crates/nanovm/src/lease.rs:131-137` (`parse_url_opts(url, std::env::vars())`); `M:ship.rs:74`, `B:ship.rs:89`; `M:config.rs:40-44,163-167`, `B:config.rs:46-50,222-226`; `B:deploy/systemd/nanovm-store.conf:16-18` (`EnvironmentFile=/etc/nanovm/store.env`) | Every node's supervisor (root); the operator makes it. One identity for all nodes and both stores. Until rotated by hand | Read every app's committed data in plaintext (versions are unencrypted tarballs, app-held secrets included, S6); rewrite heads and versions so nodes restore attacker-chosen state; delete every backup; forge lease records. Revoking it to fence one lost node fences all of them | `object_store`'s `aws-base` chain already takes an EC2 instance role (IMDS), web identity (`AWS_WEB_IDENTITY_TOKEN_FILE` + `AWS_ROLE_ARN`: EKS IRSA, MinIO STS via `AWS_ENDPOINT_URL_STS`), an ECS task role and EKS Pod Identity. Set those and drop the keys. A role per node makes fencing per node. Off-cloud: IAM Roles Anywhere's helper serving an IMDS-style endpoint, with short-lived certs | **No** for AWS and MinIO (env vars only). GCS or Azure native identity: enable `object_store`'s `gcp-base`/`azure-base` (`B:crates/nanovm/Cargo.toml:69`) plus tests. Per-store credentials, a no-delete shipping role, or encrypting versions: new code |
| S2 | **High** | B | App secrets directory: `/etc/nanovm/secrets/<app>.json`, plaintext JSON per app (B-100) | `B:config.rs:97-99`; `B:secrets.rs:57-92`; `B:supervisor.rs:179,243,655-658`; `B:deploy/systemd/nanovm.toml:23-25`; `B:deploy/single-node/install.sh:24`; `B:deploy/single-node/README.md:218-244` | The operator, for every tenant; each node's disk (copied to every node by hand); each app's worker memory. Values come from developers out of band. Whatever the provider issued (usually never expires); rotated by hand | Root on any node, or the operator's channel with developers, yields every tenant's third-party keys (LLM keys, OAuth client secrets, bot tokens). A malicious signed version of an app can send its own keys out over egress | Today: fill the directory from a secret manager using the node's own identity (Vault Agent templates, a cloud secret manager, `LoadCredentialEncrypted=` sealed to the TPM). nanovm already restarts a worker when its file changes. Secretless for cloud APIs: per-app workload identity tokens that AWS STS, GCP Workload Identity Federation and Entra accept. For APIs that take only a static key: the host adds the key on egress, so the guest never sees it | **No** for secret-manager-fed files. **Yes** for per-app OIDC tokens (an issuer, its signing key in a KMS or rotated hourly, a JWKS URL) and for egress-side injection |
| S3 | **High** (absence) | M+B | Proxy↔node, both ways: no authentication. The node trusts `nanovm-app`/`-trust`/`-host`/`-callers`; the proxy's `publish_listen` trusts `nanovm-app`/`-host`/`-callers` | `M:proxy.rs:69`, `B:proxy.rs:75` (plain `HttpConnector`); `B:registry.rs:76-83`; `B:proxy.rs:386-422,527-533`; `B:supervisor.rs:780-786,996-1010`; `B:deploy/systemd/nanovm.toml:4-5`, `B:deploy/systemd/proxy.toml:9-10` | Nobody: trust is network position (loopback on one host, "a private network" with several). Permanent | Anyone who reaches node `:8080` or proxy `:8444` (any local process on a single-node VM; any peer on the network with several nodes) runs any Sigstore-signed app whatever the `signers`/`hosts` lists say, forges `nanovm-callers` into apps' internal routes, publishes into any app's WebSockets, and skips rate limits | Keep it secretless. mTLS with short-lived SPIFFE certificates (SPIRE, rotated hourly) through a sidecar such as ghostunnel or Envoy; on one host, an nftables `socket cgroupv2` rule so only `nanovm-proxy.service` reaches `:8080` and only `nanovm.service` reaches `:8444`. Don't add a shared bearer token | **No** with a sidecar or nftables. Native rustls mTLS from rotated cert files, or Unix sockets with peer credentials: new code |
| S4 | **High** | M+B | Registry push credential for `nanovm publish`: `NANOVM_REGISTRY_USER`/`NANOVM_REGISTRY_TOKEN` | `M:crates/nanovm/src/main.rs:58-59,78`, `B:main.rs:61-62,82`; `B:registry.rs:436-461,491-527`; `M+B:README.md` (Publishing) | A developer's laptop: for GHCR a classic PAT with `write:packages` (GitHub's page ticks `repo` too). Or CI: `GITHUB_TOKEN`. PAT until it expires or is revoked; `GITHUB_TOKEN` one job | Moving a tag is a deploy: roll any host back to any older signed version (unless its record sets `min=`), or delete packages. With `repo` scope: push to the signing branch and let CI sign arbitrary code as the trusted identity | Publish only from CI (`publish-app.yml` already does, with `GITHUB_TOKEN`). For ECR, Artifact Registry or ACR, exchange GitHub's OIDC token for a 1-12 h registry token and pass it as `NANOVM_REGISTRY_TOKEN` | **No** (nanovm already passes the login to the token realm or as Basic). Docker credential helpers (`docker-credential-ecr-login`, `-gcr`) would be new code |
| S5 | **High** | M | Secrets in public packages: oauth's `env.CLIENT_SECRET`, gateway's `UPSTREAM_KEY` in `env` | `M:examples/oauth/nanovm.json:9`, `M:examples/oauth/src/lib.rs:81`; `M:examples/gateway/README.md:16-18,48-49`, `M:examples/gateway/src/lib.rs:48` | Anyone who can pull (pulls are anonymous). For ever (copies are cached) | The app's upstream accounts | Beta's secrets mechanism and publish refusal (`B:secrets.rs:97-125`, `B:main.rs:70`) | Done on B |
| S6 | **Medium** | M+B | Credentials apps keep in object data, shipped in plaintext: oauth's raw session ids as KV keys (30-day bearer), webhooks' HMAC secret | `M:examples/oauth/src/lib.rs:98,133`, `B:…:103,138`; `M:examples/webhooks/src/lib.rs:49`, `B:…:51` | The object's volume, every version shipped to the store, any copy of either. 30 days (sessions); until reconfigured (webhooks) | With S1 or a node's disk: hijack any signed-in user's session; forge webhook deliveries | Store `sha256(session)` as the key, not the id. Verify webhooks with the sender's public key where it offers one (Standard Webhooks `v1a`, Discord); GitHub signs with a shared secret only | Examples only. Encrypting shipped versions: new code |
| S7 | **Medium** | M+B | Telemetry backend credentials in the Collector; the node→Collector hop is unauthenticated | `M:telemetry.rs:258-262`, `B:telemetry.rs:267-271` (no auth sent, credentials in the URL refused); `M+B:demo/otel-collector.yaml:2-3`; `B:deploy/single-node/README.md:335-339` | The Collector's config (operator). Vendor API keys, usually non-expiring | Write (often read) the backend: guests' stdout, stderr and request paths. Anyone reaching `:4318` injects or floods telemetry | Collector auth extensions using the host's cloud identity: `sigv4auth` (AWS), `googleclientauth` (Google Cloud), `azureauth` (Azure). A Collector on each node's loopback, forwarding to a gateway over mTLS with short-lived certs | **No** for Collector→backend. The node presenting a client certificate to a remote Collector: new code |
| S8 | **Medium** (planned) | B | Designs that would add long-lived secrets: B-102 (registry pull credentials in `registries.json` on every node), B-103 (secrets "sealed to the node's public key": a long-lived node private key) | `B:ISSUES.md:1148-1154`; `B:notes/beta-secrets.md:164-179` | Each node. Indefinite | B-102: private packages' code. B-103: every secret ever sealed to that node | Pull with the node's identity (ECR via an instance role, Artifact Registry via the metadata server). Seal to a KMS key the node may use through its identity, or have nodes fetch from a secret manager with a policy per app | **Yes** either way: choose the design now |
| S9 | **Medium** | M | restic repository password and repository credentials | `M:README.md:430-463`; `M:scripts/backup.py:24,102-103` | Operator or node. The password lives as long as the backups | Decrypt every backup (password); read or delete the repository (credentials) | `RESTIC_PASSWORD_COMMAND` fetching from a KMS or secret manager with the node's identity; repository access through an instance role | **No**. Removed on B, where S1 replaces it without encryption |
| S10 | **Low** | M+B | Deploy authority: DNS zone access, signer identities, the embedded Sigstore root. The TXT record itself is **not** signed; the OCI artifact is (keyless) | `M+B:proxy.rs:1-17`; `B:proxy.rs:436-465` (DoH, no DNSSEC check); `M:registry.rs:213`, `B:registry.rs:217`; `B:config.rs:180-184`, `B:deploy/systemd/proxy.toml:18-21` | DNS-provider login or API token (outside nanovm, indefinite). Signing certificates live 10 minutes | DNS control routes a host to any app whose signer the record trusts (on M any signer: there is no allowlist), but never to another repository's data | Already keyless. Protect the DNS account (SSO, hardware 2FA, registrar lock, DNSSEC); keep `signers` exact rather than `org/*` | **No** (DNSSEC validation would be new code) |
| S11 | **Low** | M+B | CI tokens: only `GITHUB_TOKEN`/`github.token` and the OIDC token for attestations. No repository secrets | `M+B:.github/workflows/publish-app.yml:31-35,55-56`; `M+B:publish-guests.yml:14-18,38`; `B:bench-history.yml:46`; `B:release.yml.disabled:54-57,80` | GitHub Actions runner. One job; OIDC minutes | Push packages and sign as the workflow during that job | Already short-lived. Pin third-party actions by SHA in jobs with `id-token: write`; pass the token through `env`, not `${{ }}` in the script | **No** |
| S12 | **Low** | B | TLS keys: Caddy's ACME account key and certificate keys | `B:deploy/single-node/Caddyfile:1-13`; `B:deploy/single-node/README.md:149-164` | Caddy on the VM. Certificates 90 days, a new key each renewal (Caddy's default); the account key indefinite | Impersonate app hosts until expiry (needs network position) | Already automated. Shorter with a CA's short-lived profile, or terminate at a managed load balancer | **No** |
| S13 | **Low** | B | Store credentials in containers: the image holds none, but `-e`/`--env-file` would show them to `docker inspect` | `B:deploy/container/README.md:13-56`; `B:deploy/container/Dockerfile` | Docker host. Indefinite | As S1 | Instance role with IMDSv2 hop limit 2, or IRSA/Pod Identity on Kubernetes | **No** |
| S14 | **Low** | B | Local `nanovm run --secrets secrets.json` | `B:main.rs:102-136`; `B:README.md:475` | Developer's laptop, plaintext. Indefinite | That developer's dev keys | Use dev-only keys; keep the file out of version control (documented) | **No** |
| S15 | Info | M+B | Journal record MAC key | `M:journal.rs:20-30,758`, `B:journal.rs:20-30,844` | Per object, random, made by the node, never leaves the log | Forge undo records for one object (a worker can already write its volume) | Not a credential: nobody provisions or holds it | No action |
| S16 | Info (secretless) | B | Admin socket | `B:admin.rs:1-53` | Root-only 0700 directory; no token | — | Already secretless | Keep |
| S17 | Info (secretless) | M+B | Registry pulls, tag resolution and DoH lookups | `B:registry.rs:353-434`; `B:proxy.rs:436-475` | Anonymous | — | Already secretless; packages must be public (B-102) | Keep |

Who holds what:

- **The node:** S1, S2, S8 (planned). The workers get only their own app's S2 values, and an empty
  environment (`M:supervisor.rs:587`, `B:supervisor.rs:837`).
- **The proxy:** nothing. It pulls anonymously, asks a public DoH resolver and sends telemetry
  without credentials. S3 is about what it trusts.
- **The operator:** S1, S2, S7, S9 (M), S12, and the `signers` list (S10).
- **The developer:** S4, S10 (DNS account), S14, and a short-lived OIDC sign-in for keyless
  signing.
- **An app:** its S2 values, and S6 in its own data.
- **CI:** S11 only.

## The three most serious

1. **S1, the object-store credential.** One static key pair, shared by every node and by both the
   lease store and the capture store, reads and rewrites every app's data and backups. It is also
   the easiest to remove: `object_store` already supports instance roles, web identity and
   container credentials, so on AWS or MinIO it is a deployment change with no new code.
2. **S2, the beta secrets directory.** It makes the operator, and every node's disk, the custodian
   of every tenant's third-party keys in plaintext. Removing those keys from apps needs per-app
   workload identity (for cloud APIs) or egress-side injection (for key-only APIs). Both are new
   code. Feeding the directory from a secret manager needs none.
3. **S3, proxy↔node trust by network position.** It holds no secret, but anyone who reaches a
   node's port bypasses the beta's `signers` and `hosts` allowlists and can forge internal calls.
   The secretless fix (short-lived mTLS through a sidecar, or an nftables cgroup rule on one host)
   needs no nanovm code. A shared token would add a new long-lived secret.

Details follow in the next commit.
