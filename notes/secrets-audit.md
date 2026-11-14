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

## Details

### S1. The object-store credential (leases and capture store)

**What and where.**
- `leases` (`M:config.rs:40-44`, `B:config.rs:46-50`) and `[capture] store` (`M:config.rs:163-167`,
  `B:config.rs:222-226`) are object-store URLs. `Leases::open` (`M+B:lease.rs:131-137`) opens
  both with `object_store::parse_url_opts(url, std::env::vars())`: every environment variable
  whose lower-cased name is an S3 config key configures the client. `ship::store` opens
  `capture.store` through it (`M:ship.rs:74`, `B:ship.rs:89`).
- Only `aws-base` is compiled (`M:crates/nanovm/Cargo.toml:65`, `B:…:69`): `s3://` works;
  `gs://` and `az://` fail with "feature not enabled".
- Beta's deploy material puts static keys in `/etc/nanovm/store.env`, loaded with
  `EnvironmentFile=` (`B:deploy/systemd/nanovm-store.conf:16-18`; documented in
  `B:deploy/systemd/README.md:47-48`, `B:deploy/single-node/README.md:295-300` and
  `B:notes/recovery-runbook.md:13-16`). Both READMEs say "credentials and region from `AWS_*`".
- There is no `LoadCredential=` on either ref.

**Who holds it, and for how long.** The supervisor of every node, as root. Workers never see it:
they start with an empty environment (`M:supervisor.rs:587`, `B:supervisor.rs:837`). The
environment is per process, so one identity serves the lease store and the capture store, and
the reference setup copies it to every node. It lives until someone rotates it.

**If it is stolen.**
- **Read.** Every version of every object of every app is a plain tar of its volume; neither ref
  encrypts anything. That includes what apps keep in their data (S6).
- **Write.** A version is `<app>/<object>/<seq>-<sha256>.tar`, and the object's `head` names the
  one that counts. With write access, an attacker can put a tar under its own hash and move the
  head. Nodes restore the head on takeover (`ship::claim`) and on `admin restore`, so
  attacker-chosen state lands in any app's object.
- **Delete.** `admin delete` and `delete-app` remove from the store, so the credential has delete
  rights, and can delete every backup (B-800).
- **Leases.** Forged `nodes/<node>` or `objects/<app>/<object>` records break the one-instance
  guarantee: two live instances of an object, or none.
- **Fencing.** The recovery runbook fences a lost node by revoking "its store credentials"
  (`B:notes/recovery-runbook.md:90-91`). With one shared key, that stops every node.

**Options today, with no new code.** `object_store` 0.14.2's `AmazonS3Builder::build`
(`src/aws/builder.rs:1168-1255` in the vendored source) tries, in order: static keys, web
identity, an ECS task role, EKS Pod Identity, then the instance metadata service. So:
- **EC2:** attach an instance profile and set no keys. Credentials come from IMDSv2, are temporary and rotate
  automatically. [EC2 roles](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/iam-roles-for-amazon-ec2.html)
- **EKS:** IRSA (`AWS_WEB_IDENTITY_TOKEN_FILE` + `AWS_ROLE_ARN`), or Pod Identity
  (`AWS_CONTAINER_CREDENTIALS_FULL_URI` + `AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE`).
  [IRSA](https://docs.aws.amazon.com/eks/latest/userguide/iam-roles-for-service-accounts.html),
  [Pod Identity](https://docs.aws.amazon.com/eks/latest/userguide/pod-identities.html)
- **ECS:** `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI`, which ECS sets itself.
- **MinIO:** `AssumeRoleWithWebIdentity` against MinIO's own STS (`AWS_ENDPOINT_URL_STS`), with
  any OIDC token MinIO trusts in the token file (Kubernetes, SPIRE's JWT-SVIDs written by
  spiffe-helper, a cloud's identity token).
  [MinIO STS](https://min.io/docs/minio/linux/developers/security-token-service/AssumeRoleWithWebIdentity.html)
- **A VM outside AWS:** IAM Roles Anywhere's `aws_signing_helper serve` answers like IMDS on
  localhost; point `AWS_METADATA_ENDPOINT` at it. Its key can live in a TPM and its certificate
  can be short-lived (SPIRE, step-ca).
  [Roles Anywhere helper](https://docs.aws.amazon.com/rolesanywhere/latest/userguide/credential-helper.html)
- **Per node:** give each node its own role, limited to the bucket prefix. Then fencing a node
  revokes only its role.
- **The unit:** drop the `EnvironmentFile=`. IMDS is reachable, since
  `RestrictAddressFamilies=` allows `AF_INET`.

**What needs code, or has no option.**
- **GCS and Azure** with their own identity (metadata server, GCP Workload Identity Federation,
  managed identity): add `gcp-base`/`azure-base` to `object_store`'s features (one line) and a test
  per scheme. `config.rs`'s shared-store check already accepts any URL other than `memory:`,
  `file:` and `/`. Today GCS works only through its S3 interoperability API, with HMAC keys
  (static).
- **Cloudflare R2, Backblaze B2 and most S3-compatibles** issue only static keys. R2's temporary
  credentials are minted with a parent API token, which only moves the long-lived secret.
- **Least privilege.** Leases need conditional puts under `nodes/` and `objects/`; shipping needs
  put-if-absent and conditional head writes; only `admin delete` needs delete. One process-wide
  identity can't split these. A credential per store URL would be new code.
- **Encryption.** Store read access stops meaning data access only if versions are encrypted
  before they ship (a data key per app, wrapped by a KMS key the node reaches through its
  identity). New code.

### S2. The beta secrets directory

**What and where.**
- `secrets = "/etc/nanovm/secrets"` (`B:config.rs:97-99`, `B:deploy/systemd/nanovm.toml:23-25`),
  made `0700` by `B:deploy/single-node/install.sh:24` and checked at start (`B:supervisor.rs:179`,
  `B:secrets.rs:85-92`).
- Each app's `<app>.json` is read when its worker starts (`B:supervisor.rs:655-658`,
  `B:secrets.rs:57-75`), checked for changes every second (`B:supervisor.rs:243`), sent on the
  worker's stdin, and served through `wasi:config` (`B:app.rs:175`).
- Design and docs: `B:notes/beta-secrets.md`, `B:README.md:464-478`,
  `B:deploy/single-node/README.md:218-244`. Containers mount the directory read-only
  (`B:deploy/container/README.md:46-49`).

**Who holds it, and for how long.**
- **The operator**, for every tenant. Developers hand values over out of band; they can't set
  their own until deploys are authenticated (B-103).
- **Each node's disk**, in plaintext. With several nodes, "each reads its own directory; nothing
  copies secrets between them" (`B:notes/beta-secrets.md:167`), so there are as many copies as
  nodes.
- **Each app's worker memory.** It is not dumpable only when the supervisor runs as root (B-104).
- **Lifetime** is whatever the provider issued. OAuth client secrets, LLM API keys and bot tokens
  usually never expire. Rotation is by hand; the node restarts the worker when the file changes.

**If it is stolen.** Root on any node, a backup of `/etc/nanovm`, or the channel between the
operator and developers gives every tenant's keys to their upstream services. A malicious version
of an app, signed by an identity its record trusts, reads its own keys and can send them wherever
its egress reaches. Masking hides only exact values in telemetry and error text (B-101).

**Keep what beta already does.** Secrets are never in the package (`check_package`), never in
argv or the environment, and never copied verbatim into errors or telemetry. Each app gets only
its own, behind its uid and Landlock. A missing secret fails closed, and a changed file restarts
the worker.

**Options today, with no new code.**
- **Fill the directory from a secret manager**, authenticated by the node's own identity: Vault
  Agent templates (with AWS, GCP or Kubernetes auth), or a timer that reads AWS Secrets Manager or
  GCP Secret Manager with the instance role. Nothing long-lived is then copied by hand. nanovm
  already picks up changes. [Vault Agent templates](https://developer.hashicorp.com/vault/docs/agent-and-proxy/agent/template)
- **Short-lived values where the upstream allows them** (Vault's dynamic database credentials,
  for example) work too, since a change restarts the worker.
- **At rest:** systemd's `LoadCredentialEncrypted=` keeps the files sealed to the TPM. Set
  `secrets` to the unit's credentials directory, check that its mode passes `check_dir`, and
  expect a rotation to restart the unit. [systemd credentials](https://systemd.io/CREDENTIALS/)

**Secretless, with new code.**
- **Per-app workload identity.** The node, or the proxy, acts as an OIDC issuer. Each worker gets
  a short-lived JWT whose subject is the app's name (host and repository), refreshed through
  `wasi:config` or a host function. Apps exchange it at AWS STS (`AssumeRoleWithWebIdentity`),
  GCP Workload Identity Federation or Entra workload identity federation, so they need no API
  keys for those clouds. It needs an issuer signing key (in a KMS, or rotated hourly with old keys
  left in the JWKS until their tokens expire), a public JWKS URL, and token refresh.
  [AWS](https://docs.aws.amazon.com/STS/latest/APIReference/API_AssumeRoleWithWebIdentity.html),
  [GCP](https://cloud.google.com/iam/docs/workload-identity-federation),
  [Entra](https://learn.microsoft.com/en-us/entra/workload-id/workload-identity-federation)
- **Egress-side injection** for APIs that take only a static key. `egress.rs` already builds every
  outbound request on the host, so the supervisor's forwarder could add an operator-held
  `Authorization` header for one (app, host) pair. The guest never holds the key, so a malicious
  version can use it only through that host and can't take it away. The key is still long-lived,
  but it never enters the guest.
- B-103's plan (sealing to a node's public key) adds a long-lived node key: see S8.

**What has no secretless option.** Third-party APIs that issue only static keys (most LLM
providers, many bot platforms) and GitHub's web sign-in, which takes a client secret. These can
be kept out of the guest and out of hand-copied files, but not removed.

### S3. Proxy↔node trust by network position

**What and where.**
- **Proxy to node:** plain HTTP through hyper's `HttpConnector` (`M:proxy.rs:69`,
  `B:proxy.rs:75`; sent at `B:proxy.rs:301-303,331-335`). The node takes the app, its trust policy,
  its version floor and its host from `nanovm-*` headers (`B:registry.rs:76-83,106-149`,
  `B:supervisor.rs:441-447`). It also trusts WebSocket events and `nanovm-callers`. W-602 says it
  plainly: "a node reached without the proxy trusts what it is sent".
- **Node to proxy:** the supervisor posts publishes and calls between objects to `cfg.proxy`
  (`B:supervisor.rs:780-786,996-1010`). The proxy's `publish_listen` serves `/publish` and
  `/objects/…`, trusting `nanovm-app`, `nanovm-host` and `nanovm-callers`
  (`B:proxy.rs:386-422,527-533`: "network policy must restrict it to nodes").
- **Docs:** `B:deploy/systemd/nanovm.toml:4-5`, `B:deploy/systemd/proxy.toml:9-10`,
  `B:README.md:361,493`. `M+B:PLAN.md`: "The node has no TLS and no auth of its own", and
  "Multi-node operation still needs authenticated ingress/internal-call placement".
- **Rate limits** trust `X-Forwarded-For` from the addresses in `[clients] trusted`
  (`B:config.rs:203-207`). That is network position too.

**Who holds it.** Nobody; there is no secret. On one host both ports are on loopback. With
several nodes, each node's `listen` and the proxy's `publish_listen` face a network.

**If an attacker reaches the node's port:**
- They can ask for any app version signed by any Sigstore identity, naming their own
  `nanovm-trust`. The beta's `signers` and `hosts` lists live only in the proxy, so they are
  bypassed. The node still checks the signature, and data is keyed by host and repository, so
  another repository's data stays out of reach.
- They can drive a host's own app with a forged `nanovm-callers`, reaching routes the app keeps
  for its own objects.
- They skip the proxy's per-client and per-app rate limits.

**If an attacker reaches `publish_listen`:** they can publish into any app's WebSockets by naming
it, and call any app's objects as if they were one of its own.

On a single-node VM, "an attacker" means any local process, of any uid: the units restrict
neither addresses nor uids. Workers can't, because their jail allows no sockets.

**Options today, with no new code** (all secretless):
- **One host:** an nftables `output` rule that admits loopback connections to `:8080` only from
  sockets in `nanovm-proxy.service`'s cgroup, and to `:8444` only from `nanovm.service`'s
  (`socket cgroupv2`; check it on the target kernel). The container setup already gets the same
  result by sharing one network namespace (`B:deploy/container/README.md:39`).
- **Several hosts:** a sidecar doing mTLS with short-lived certificates. SPIRE issues X.509-SVIDs
  (one hour by default) and rotates them; ghostunnel or Envoy terminates and originates mTLS and
  checks the peer's SPIFFE ID; nanovm keeps speaking plain HTTP to loopback.
  [SPIRE](https://spiffe.io/docs/latest/spire-about/), [ghostunnel](https://github.com/ghostunnel/ghostunnel)
- **Or** a private network whose security groups admit only proxies to `:8080` and only nodes to
  `:8444`. That is secretless but coarse.
- A WireGuard mesh would add long-lived static keys; prefer SPIFFE.

**With new code:** native mTLS with rustls (already a dependency, `B:Cargo.toml:34-35`), reading a
certificate that spiffe-helper rotates, or the SPIFFE Workload API, and checking the peer's
SPIFFE ID in both directions. On one host, Unix sockets with peer credentials, as the admin
socket's directory check already does. Not a shared bearer token: that would be a new long-lived
secret on every node and proxy.

### S4. Registry push credentials

**What and where.**
- `nanovm publish` reads `NANOVM_REGISTRY_USER` and `NANOVM_REGISTRY_TOKEN` (`M:main.rs:58-59,78`,
  `B:main.rs:61-62,82`).
- `Oci::push_app` asks `/v2/` how to authenticate. On a Bearer challenge it sends the login as
  Basic to the token realm (https or loopback only) and gets a token for
  `repository:<name>:pull,push`. On a Basic challenge it sends the login with every request, to
  the registry's origin only (`B:registry.rs:436-461,491-527,569-570`).
- The workflows pass `github.actor` and `GITHUB_TOKEN` (`M+B:publish-app.yml:55-56`).
  `publish-guests.yml:38` uses `oras login` with the same token.

**Who holds it, and for how long.**
- **CI:** `GITHUB_TOKEN`, limited to `packages: write` by the workflow. It expires when the job
  ends (24 hours at most).
- **A developer publishing from a laptop:** for GHCR, a classic personal access token with
  `write:packages`. GitHub Packages takes classic tokens, and GitHub's page ticks `repo` too when
  `write:packages` is ticked. It lives as long as it was created for, often with no expiry.
  [GHCR auth](https://docs.github.com/en/packages/working-with-a-github-packages-registry/working-with-the-container-registry#authenticating-with-a-personal-access-token-classic)

**If it is stolen.**
- **Moving a tag is a deploy**: the proxy resolves `app=<repo>:<tag>`. Nodes run only signed
  versions, so a stolen push credential can't run new code. But it can move a host back to any
  older signed version, unless the record sets `min=`, and it can delete or overwrite tags.
- **With `repo` scope**, it can push to the branch the trusted workflow runs on and dispatch
  `publish-app.yml`. CI then signs the attacker's code as the trusted identity: arbitrary code
  under the app's host, with its data.

**Options today, with no new code.**
- **Publish only from CI**, as the template does; then no developer holds a push credential.
- **Other registries through GitHub's OIDC token**, passed in the existing variables:
  - ECR: `aws-actions/configure-aws-credentials` with a role, then `NANOVM_REGISTRY_USER=AWS` and
    `NANOVM_REGISTRY_TOKEN=$(aws ecr get-login-password)` (12 hours).
  - Artifact Registry: `google-github-actions/auth` with Workload Identity Federation, then user
    `oauth2accesstoken` and `gcloud auth print-access-token` (1 hour).
  - ACR: `azure/login` with a federated credential, then `az acr login --expose-token` (about 3
    hours).
  - nanovm passes the login as Basic either way, so these work as they are.
    [GitHub OIDC](https://docs.github.com/en/actions/security-for-github-actions/security-hardening-your-deployments/about-security-hardening-with-openid-connect),
    [ECR](https://docs.aws.amazon.com/AmazonECR/latest/userguide/registry_auth.html),
    [Artifact Registry](https://cloud.google.com/artifact-registry/docs/docker/authentication),
    [ACR](https://learn.microsoft.com/en-us/azure/container-registry/container-registry-authentication)
- **From a laptop**, the same commands give 1-12 hour tokens from the developer's SSO session
  instead of a stored token. GHCR has no such exchange: from a laptop it needs a PAT.
- **Set `min=`** on records, so a moved tag can't go below the floor.

**With new code:** support Docker credential helpers (`credHelpers` in `~/.docker/config.json`),
so `docker-credential-ecr-login` or `docker-credential-gcr` mint a token for each push from the
developer's identity; or have `nanovm publish` exchange a GitHub OIDC token itself.

The publishing job holds `packages: write` and `id-token: write` while it runs third-party actions
and `cargo install`; see S11.

### S5. Secrets in public packages (main)

- **On main,** an app's only inputs are its manifest's `env` and `config`, published as the
  package's config blob and pulled anonymously.
- **The oauth example** ships `"env": { "CLIENT_SECRET": "set-me" }`
  (`M:examples/oauth/nanovm.json:9`, read at `M:examples/oauth/src/lib.rs:81`).
- **The gateway's README** tells developers to put the API key in `env` as `UPSTREAM_KEY`
  (`M:examples/gateway/README.md:16-18`, read at `M:examples/gateway/src/lib.rs:48`), and says it
  is published (`:48-49`).
- `nanovm publish` on main checks nothing, so anyone who pulls has the key, for as long as any
  copy exists.
- **Fixed on beta:** the manifest lists `secrets`, the node gives the values (S2), and
  `check_package` refuses credential-like keys and values at publish (`B:secrets.rs:97-125`,
  called at `B:main.rs:70`). Landing beta fixes it; S2's caveats then apply.

### S6. Credentials apps keep in their own data

- **oauth.** The session id is the credential: 64 random hex digits in a cookie, valid 30 days
  (`SESSION_MS`). It is stored as the KV key `session:{id}`, in `auth` and in each user's object
  (`M:examples/oauth/src/lib.rs:98,133`; `B:…:103,138`).
  - KV keys are file names under `/data/.kv/` (`B:README.md:386`), so every shipped version of
    those objects holds every live session id in plaintext.
  - Anyone with the store credential (S1), a node's disk or a backup can sign in as any user.
  - Fix in the example, with no nanovm code: store `sha256(id)` and look sessions up by the hash.
    A leaked store then holds nothing a browser can present.
- **webhooks.** Each endpoint's HMAC secret is stored in its KV `config`
  (`M:examples/webhooks/src/lib.rs:49`, `B:…:51`), and ships with every version.
  `PUT /{hook}/config` is unauthenticated, as its README says.
  - GitHub signs deliveries with a shared secret only, so a secret is unavoidable for GitHub.
  - Senders that implement Standard Webhooks' asymmetric `v1a` signatures (Ed25519), or Discord's
    interactions, can be checked with a public key alone.
    [Standard Webhooks](https://github.com/standard-webhooks/standard-webhooks/blob/main/spec/standard-webhooks.md)
- **jobs and locks** hand out random fencing tokens for leases. They are not credentials worth
  stealing.
- **In general,** whatever an app keeps goes to the store unencrypted. Encrypting versions before
  they ship would make S1's read access useless; that is new code (S1).

### S7. Telemetry: Collector credentials and the unauthenticated hop

- **No telemetry secret in nanovm.** The node and the proxy send OTLP/HTTP with no credentials:
  `Relay::new` refuses an endpoint with a user, password, query or fragment, and `send` adds only
  `content-type` (`M:telemetry.rs:258-285`, `B:telemetry.rs:267-293`). Only the supervisor
  connects; workers go through it. The backend credential lives in the Collector's exporter, by
  design (`M+B:demo/otel-collector.yaml:1-3`, `B:deploy/single-node/README.md:335-339`).
- **Who and how long:** the Collector's host. Vendor API keys (Grafana Cloud, Honeycomb, Datadog
  and similar) usually don't expire.
- **If it is stolen:** write access to all telemetry, and with many vendors read access too:
  guests' stdout and stderr, request paths, error text (declared secrets masked, exact matches
  only).
- **The node→Collector hop is unauthenticated**, so anyone reaching `:4318` can inject spans and
  logs (fake errors, alert noise) or flood it. The reference setup keeps it on loopback.
- **Options today, with no new code:**
  - Collector extensions that use the host's cloud identity instead of a key: `sigv4auth` (AWS:
    Amazon Managed Prometheus, X-Ray, CloudWatch), `googleclientauth` (Google Cloud), `azureauth`
    (Azure Monitor, managed identity). Vendors that accept only API keys keep one, on the Collector
    alone. [Collector extensions](https://github.com/open-telemetry/opentelemetry-collector-contrib/tree/main/extension)
  - Across hosts: a Collector on each node's loopback, forwarding to a gateway Collector over mTLS
    with short-lived certificates (the Collector reads certificate files; SPIRE or cert-manager
    rotate them).
- **With new code:** the node presenting a client certificate to a remote Collector, reloaded when
  it rotates.

### S8. Planned designs that would add long-lived secrets (beta)

- **B-102** (`B:ISSUES.md:1148-1150`) plans private registry pulls with "registry credentials per
  registry host, in the same root-only directory (for example `registries.json`)". Every node
  would hold static pull credentials.
  - Instead: ECR pulls with the node's instance role (`ecr:GetAuthorizationToken`, 12-hour
    tokens), and Artifact Registry with the metadata server's one-hour token. Both fit
    `registry::Oci`'s existing path (a login sent as Basic to the token realm).
  - GHCR's private pulls from outside GitHub need a PAT; there is no workload identity. So keep
    packages public, as now, or use a registry that accepts identities.
- **B-103** (`B:ISSUES.md:1152-1154`, `B:notes/beta-secrets.md:176-179`) plans for developers to
  set their own secrets "sealed to the node's public key". That adds a long-lived private key on
  every node, and every secret ever sealed to it falls with it.
  - Instead: seal to a KMS key (AWS KMS, Cloud KMS) whose decrypt right is granted to nodes'
    identities.
  - Or keep each app's secrets in a secret manager. Through the provider's OIDC federation, the
    deploy identity (the `<issuer> <subject>` the trust policy names) may write only that app's
    path, and nodes may read it.

### S9. restic password and repository credentials (main)

- **What and where:** main's backup is `scripts/backup.py`, a wrapper around restic
  (`M:README.md:430-463`, `M:scripts/backup.py:24,102-103`). The operator sets
  `RESTIC_REPOSITORY`, `RESTIC_PASSWORD_FILE` and the repository's own credentials (for S3,
  `AWS_*` again).
- **Lifetime:** the password is the encryption key of every snapshot, so it must live as long as
  they do.
- **Options:** `RESTIC_PASSWORD_COMMAND` can read the password at run time from a KMS-backed
  secret manager with the node's identity, so no copy sits on disk. An S3 repository can be
  reached with an instance role.
- **On beta,** restic is gone (`B:notes/beta-recovery.md:9-25`). The capture store (S1) replaces
  it, without encryption, so the bucket credential now also guards the data's confidentiality.

### S10. Deploy authority: DNS, signers and the Sigstore root

- **No secret of nanovm's authorizes a deploy.**
  - The TXT record `_nanovm.<host>` (`app=…; trust=<issuer> <subject>; min=…`) is not signed.
    The proxy asks a DoH resolver for it (`B:proxy.rs:436-465`) and doesn't check DNSSEC.
  - The node checks that the OCI artifact carries a Sigstore bundle from one of the record's
    identities (`M:registry.rs:313-346`, `B:registry.rs:317-350`).
  - Signing is keyless: a GitHub Actions OIDC token (minutes) buys a 10-minute Fulcio certificate
    (`actions/attest-build-provenance`), or a developer signs in through a browser with `cosign`.
    [Artifact attestations](https://docs.github.com/en/actions/security-for-github-actions/using-artifact-attestations),
    [Sigstore keyless](https://docs.sigstore.dev/cosign/signing/overview/)
- **The long-lived credential behind deploys is the DNS account:** a registrar or DNS-provider
  login, or an API token if records are automated. The developer or the domain's owner holds it,
  outside nanovm.
  - Whoever controls a host's DNS controls its routing.
  - On main, the node accepts any identity a record names. On beta, the proxy cuts `trust=` to
    the operator's `signers` (`B:config.rs:180-184`, `B:proxy.rs:450-451`).
  - An app's data is keyed by host and repository, so a hijacked record never reaches another
    repository's data. It can still serve allowed signed code under that host.
- **Prefix signers are broad.** `https://github.com/org/*` (as in
  `B:deploy/systemd/proxy.toml:21`) lets any workflow, on any branch of any repository in the
  org, deploy wherever a record trusts it. Records should pin the workflow and the ref.
- **The trust anchor** is Sigstore's public-good root, embedded in the binary without its TUF
  client (`M:registry.rs:213`, `B:registry.rs:217`, `B:Cargo.toml:50-51`). These are public keys,
  not secrets, but long-lived: when Sigstore rotates them, nodes need a new build.
- **Options:**
  - Signing is already keyless.
  - Protect the DNS account with SSO and hardware keys, and a registrar lock.
  - Sign the zone with DNSSEC and use a validating resolver. Checking the answer's AD bit in the
    proxy would be new code.
  - Change records through infrastructure-as-code with OIDC (for example Route 53 through a
    GitHub OIDC role), so no DNS API token is stored. Providers that issue only static API tokens
    can't do this.

### S11. CI tokens

- **No repository secrets** are used on either ref. Workflows use `GITHUB_TOKEN`/`github.token`
  (`M+B:publish-app.yml:56`, `M+B:publish-guests.yml:38`, `B:bench-history.yml:46`,
  `B:release.yml.disabled:80`) and the job's OIDC token for attestations (`id-token: write` at
  `M+B:publish-app.yml:34`, `M+B:publish-guests.yml:17`, `B:release.yml.disabled:56`). `ci.yml` has
  only `contents: read`.
- **Lifetime:** the job, 24 hours at most; OIDC tokens last minutes.
- **Two tidy-ups, no code:**
  - `publish-guests.yml:38` writes `${{ secrets.GITHUB_TOKEN }}` into the script's text. Pass it
    through `env:` to `--password-stdin` instead.
  - The jobs with `packages: write` and `id-token: write` run third-party actions by mutable tag:
    `actions/checkout@v5`, `dtolnay/rust-toolchain@stable`, `Swatinem/rust-cache@v2`,
    `oras-project/setup-oras@v1`, `actions/attest-build-provenance@v3`. Pin them by commit SHA,
    or sign in a separate job that only receives the digest and attests it.
    [Hardening for GitHub Actions](https://docs.github.com/en/actions/security-for-github-actions/security-guides/security-hardening-for-github-actions)

### S12. TLS keys (beta)

- **What and where:** Caddy holds an ACME account key and the certificates' keys
  (`B:deploy/single-node/Caddyfile:1-13`, `B:deploy/single-node/README.md:149-164`).
- **Lifetime:** Let's Encrypt certificates last 90 days, and Caddy makes a new key at each renewal
  unless `reuse_private_keys` is set. The account key lasts until replaced.
- **If stolen:** a certificate key lets someone with a network position impersonate the app hosts
  until it expires. The account key alone can't get certificates without passing a challenge.
- **Options:** already automated and short. Shorter with a CA's short-lived certificate profile;
  or terminate at a managed load balancer, whose provider holds the keys.
- **nanovm itself holds no TLS keys.** The proxy and the node speak plain HTTP
  (`B:proxy.rs:521-525`, W-14). Outbound TLS (registries, DoH, egress) uses webpki roots and no
  client certificates (`M:egress.rs:524-528`, `B:egress.rs:533-537`).

### S13. Containers (beta)

- The image (`B:deploy/container/Dockerfile`) and its configs hold no secrets. App secrets are
  mounted read-only (`B:deploy/container/README.md:46-49`).
- The README doesn't say how a store credential gets in. `-e` or `--env-file` would make it
  readable by anyone who can run `docker inspect`.
- Instead, use the host's instance role (IMDSv2 needs a hop limit of 2 to reach a container), or
  IRSA or Pod Identity on Kubernetes.

### S14. Local secrets for `nanovm run` (beta)

- `nanovm run app.wasm --secrets secrets.json` reads a plaintext file and restarts when it changes
  (`B:main.rs:102-136`). The README says to keep it out of version control (`B:README.md:475`).
- The examples' READMEs write keys into `~/…-secrets.json` (`B:examples/gateway/README.md:8-9`).
- Use dev-only keys with low limits. The tests use throwaway values (`s3cret`, `test-key`).

### S15-S17. Not secrets, or already secretless

- **The journal's MAC key** (`M:journal.rs:20-30,758`, `B:journal.rs:20-30,844`): 8 random bytes
  per object, made by the node with `getentropy` and stored in the log's first block. It marks
  which undo records are genuine (a truncated keyed SHA-256). Nobody provisions or sees it, and it
  never leaves the object's log. Not a credential.
- **The admin socket** (`B:admin.rs:1-53`): a Unix socket in a `0700` directory owned by the
  supervisor's user (root). No token, and no HTTP route reaches it. On main the only admin surface
  is `/_nanovm/ready` (`M:supervisor.rs:294-300`).
- **Pulls and lookups:** nodes pull anonymously (`B:registry.rs:353-434`). The proxy resolves tags
  anonymously and asks a public DoH resolver. The cost is that packages must be public (B-102).
- **The workers' environment** is empty (`env_clear()`). They get only their own app's secrets, on
  stdin, and never the store credential.
- **Tests and scripts** hold only throwaway values: `RESTIC_PASSWORD="disposable-test-only"`
  (`M:scripts/test_backup.py:29`), `s3cret`, `test-key`, and `Basic bWU6c2VjcmV0` (`me:secret`,
  `B:registry.rs:908`). The integration tests check CI-signed guests from ghcr.io, pulled
  anonymously.

## What removing them would take

**Possible today, with no nanovm code:**
- **S1** on AWS and MinIO: instance roles, web identity, ECS or Pod Identity credentials, one role
  per node.
- **S2:** fill the directory from a secret manager, using the node's identity.
- **S3:** short-lived mTLS through a SPIFFE sidecar, or an nftables cgroup rule on one host.
- **S4:** publish only from CI; for ECR, Artifact Registry and ACR, exchange OIDC tokens for
  short-lived registry tokens.
- **S6:** hash the session ids in the oauth example.
- **S7:** Collector auth extensions that use cloud identity.
- **S9:** `RESTIC_PASSWORD_COMMAND` (main only).
- **S11:** pin actions by SHA, and pass tokens through `env`.

**New code:**
- **Per-app workload identity** (S2): nanovm as an OIDC issuer, with its key in a KMS or rotated
  hourly.
- **Egress-side key injection** (S2).
- **Native mTLS** between proxy and node (S3).
- **`gcp-base` and `azure-base`** for `object_store` (S1).
- **Encryption of shipped versions** (S1, S6).
- **Docker credential helpers** for `nanovm publish` (S4).
- **Designs for B-102 and B-103 without node-held keys** (S8).

**No secretless option:**
- Third-party APIs that issue only static keys (most LLM providers, many bot platforms).
- GitHub's OAuth client secret, and GitHub's webhook secret.
- Storage providers without temporary credentials (R2, B2 and most S3-compatibles).
- The DNS account.

These can be kept out of guests, out of packages and out of hand-copied files, and fetched by
identity from a secret manager. They can't be removed.
