# Secrets research: identity instead of secrets

The owner rejected the beta's secrets design (`origin/beta/secrets:notes/beta-secrets.md`: an
operator-placed JSON file of each app's secret values on every node) and wants no long-lived
secrets or tokens wherever possible: for apps calling out, for nodes talking to their store,
registry and telemetry, and for developers publishing. This note reports how other platforms do it
(section 1), recommends a design for nanovm (section 2), and lists the owner's decisions (section 3).
It covers every item of the parallel audit, `origin/research/secrets-audit:notes/secrets-audit.md`
(S1-S17), in [2.12](#212-what-it-replaces-audit-items-s1-s17).

Research and writing only: no code was changed, built or run.

**Sources.** Every claim links to the vendor's own documentation or specification. The sandbox
could not open most documentation hosts, so most pages were read from the vendor's public source
repository (GitHub's `github/docs`, `MicrosoftDocs/entra-docs`, `kubernetes/website`,
`spiffe/spire`, `superfly/docs`, `cloudflare/cloudflare-docs`, `denoland/docs`,
`spinframework/spin-docs`, `wasmCloud/wasmcloud.com`, `hashicorp/web-unified-docs`), AWS's API
models in `boto/botocore`, and Google's API descriptions in `googleapis/google-api-go-client`, on
2026-11-14; the links point at the published pages. Facts marked *(search)* come from a search
engine's reading of the vendor page, not from the page itself: check them before relying on them.

## Summary

- **Every app gets an identity, not a secret.** The node signs a short-lived OIDC token (a JWT,
  5 minutes) for the app it runs: issuer `https://<operator's issuer>/<host>`, subject
  `<host>/<registry>/<repository>`, any audience the app asks for, and claims for the version, the
  signer that published it and the object. AWS, Google Cloud, Vault, OpenAI, Anthropic and the
  app's own services exchange or accept it; Azure too, once ES256 is tested there (question 2). This is what GitHub Actions, Fly.io, Vercel and
  Deno Deploy do for their workloads.
- **Keys live in memory and rotate.** Each node makes its own ECDSA P-256 key at start, never
  writes it down, and replaces it daily. The proxy polls the nodes it lists for their public keys
  and serves one JWKS. Removing a node from the proxy's list revokes its key.
- **The node checks DNS before it signs**, so reaching a node's port (audit S3) is not enough to
  get another host's token.
- **A guest asks for a token with an ordinary request**, `GET http://nanovm/token?aud=…`, as it
  already calls objects at `http://nanovm/objects/…`. No new WIT interface. Or the egress layer
  attaches it to requests for a host the manifest names, so the guest never holds it.
- **Static keys (Stripe, GitHub's OAuth client secret, bot tokens) never reach the guest.** The
  supervisor, which already makes every outbound TLS connection, adds the key to requests for the
  hosts bound to it, as Fly's tokenizer and Cloudflare's outbound Workers do. The key and its
  binding live once, in the shared store, written by the developer with a keyless Sigstore
  signature by an identity the host's record trusts. Nodes hold values only in supervisor memory.
- **Nodes use their platform's identity** for the store (instance roles, IRSA, Roles Anywhere,
  MinIO STS), which `object_store` already supports: no code. Telemetry and proxy-node trust stay
  secretless (a loopback Collector with cloud-identity auth; nftables or SPIFFE mTLS).
- **It replaces** the beta's secrets directory, its file watcher, telemetry masking and secrets
  in `wasi:config`; and the plan to seal secrets to a long-lived node key (B-103).
- **Cost:** about 115 runtime lines for identity, 45 for egress injection, 40 for developer-set
  secrets: about 200 in all, no new crates. On top of `beta/integration`, removing its secrets
  code (about 100 lines) makes the net about +100.

## 1. Findings

### 1.1 Platforms that issue identity tokens to their workloads

| Platform | What the workload gets | How it asks | Subject | Lifetime | Signing |
|---|---|---|---|---|---|
| GitHub Actions | an OIDC JWT per request, any audience | `ACTIONS_ID_TOKEN_REQUEST_URL` with a bearer `ACTIONS_ID_TOKEN_REQUEST_TOKEN`, `&audience=` | `repo:org/repo:ref:…`; immutable owner and repo IDs since 2026-07-15 | 5 min in the documented example | RS256, JWKS at the issuer |
| Fly.io | an OIDC JWT per request, any audience | `POST /v1/tokens/oidc` on the Machine's Unix socket `/.fly/api` | `org:app:machine` | 10 min in the documented example | per-org issuer `https://oidc.fly.io/<org>` |
| Vercel | an OIDC JWT per project and environment | `x-vercel-oidc-token` request header in Functions, `VERCEL_OIDC_TOKEN` in builds | `owner:<team>:project:<p>:environment:<e>` | cached up to 45 min *(search)* | RS256 *(search)*; per-team issuer recommended |
| Deno Deploy | an OIDC JWT per request, any audience | `getIdToken(aud)` from `@deno/oidc` | `deployment:<org>/<app>/<context>` | 5 min; `nbf` 1 min before `iat` | ES256 keys, rotated |
| Kubernetes | a pod-bound service-account JWT | a projected volume file the kubelet refreshes | `system:serviceaccount:<ns>:<name>` | 1 h default, 10 min minimum; refreshed at 80% or 24 h | the API server's key files; external signer |
| SPIRE | X.509-SVIDs and JWT-SVIDs | the Workload API on a Unix socket, caller attested by the agent | `spiffe://<trust domain>/…` | 1 h and 5 min defaults | ec-p256 default; CA/key 24 h |

- **GitHub Actions.** A job requests a token with the runner's request URL and bearer token, and
  adds `&audience=` for the relying party
  ([reference](https://docs.github.com/en/actions/reference/security/oidc#methods-for-requesting-the-oidc-token)).
  The documented example has `iat` 1632493567 and `exp` 1632493867 (5 minutes), with `nbf` 10
  minutes before `iat` ([concepts](https://docs.github.com/en/actions/concepts/security/openid-connect#understanding-the-oidc-token)).
  Claims include `repository`, `ref`, `environment` and `job_workflow_ref`. **Lesson for nanovm:**
  because names could be recycled, "repositories created after July 15, 2026 now use an immutable
  default subject format that includes both the owner ID and repository ID"
  ([immutable subject claims](https://docs.github.com/en/actions/reference/security/oidc#immutable-subject-claims)).
  `GITHUB_TOKEN` itself expires when the job finishes, and can be refreshed for 24 hours at most
  ([GITHUB_TOKEN](https://docs.github.com/en/actions/concepts/security/github_token)).
- **Fly.io.** "Every request to the token endpoint generates a unique Json Web Token"; the
  example token's `sub` is `example-org:example-app:example-machine`, with `app_name`,
  `machine_id`, `image_digest` and `region` claims, `iat` 1712099053 and `exp` 1712099653
  (10 minutes) ([OpenID Connect](https://fly.io/docs/security/openid-connect/),
  [Tokens resource](https://fly.io/docs/machines/api/tokens-resource/)). Setting `AWS_ROLE_ARN` in
  `fly.toml` is enough for the AWS SDK; Fly's AWS trust-policy example matches `sub` with
  `StringLike "org-name:foo-bar:*"`.
- **Vercel.** The issuer is per team in the recommended mode (`https://oidc.vercel.com/<team>`),
  `sub` is `owner:[TEAM_SLUG]:project:[PROJECT_NAME]:environment:[ENVIRONMENT]`, Functions receive
  the token in the `x-vercel-oidc-token` header and builds in `VERCEL_OIDC_TOKEN`
  ([OIDC](https://vercel.com/docs/oidc), [reference](https://vercel.com/docs/oidc/reference))
  *(search)*. `@vercel/oidc`'s `getVercelOidcToken()` reads the request context, then the
  environment ([package](https://www.npmjs.com/package/@vercel/oidc)).
- **Deno Deploy.** `getIdToken("https://example.com/")` returns a JWT whose `aud` is the argument;
  "Tokens expire 5 minutes after they are issued … the tokens `nbf` claim is set to 1 minute before
  the `iat` claim"; "Deno Deploy rotates its signing keys periodically", and its keys are ES256
  (the same page also says RS256 for the tokens) ([OIDC](https://docs.deno.com/deploy/reference/oidc/)).
  Cloud Connections set up the AWS and GCP trust for the user
  ([Cloud Connections](https://docs.deno.com/deploy/reference/cloud_connections/)).
- **Kubernetes.** Projected tokens are bound to the pod, carry the requested audience, and stop
  working against the API when the pod is deleted; "the kubelet proactively requests rotation for
  the token if it is older than 80% of its total time-to-live (TTL), or if the token is older than
  24 hours", and the application must reload it
  ([service accounts](https://kubernetes.io/docs/tasks/configure-pod-container/configure-service-account/)).
  `expirationSeconds` "defaults to 1 hour and must be at least 10 minutes"
  ([projected volumes](https://kubernetes.io/docs/concepts/storage/projected-volumes/)). The API
  server publishes discovery and JWKS (`/openid/v1/jwks`) so outside parties can verify. Node
  information in pod-bound tokens "is **not** verified by the kube-apiserver"
  ([admin guide](https://kubernetes.io/docs/reference/access-authn-authz/service-accounts-admin/)).

### 1.2 Who accepts them, and what they give back

- **AWS STS `AssumeRoleWithWebIdentity`.** "Calling AssumeRoleWithWebIdentity does not require the
  use of Amazon Web Services security credentials" (the operation is `noAuth`). Credentials last
  from 900 seconds to the role's maximum (1 to 12 hours), 1 hour by default. "Tokens must be signed
  using either RSA keys (RS256, RS384, or RS512) or ECDSA keys (ES256, ES384, or ES512)." Session
  policies can narrow the role further
  ([API](https://docs.aws.amazon.com/STS/latest/APIReference/API_AssumeRoleWithWebIdentity.html),
  read from [AWS's API model](https://github.com/boto/botocore/blob/develop/botocore/data/sts/2011-06-15/service-2.json)).
  Trust policies condition on `<provider>:aud` and `<provider>:sub` (as in Fly's example above).
  STS can also map a `https://aws.amazon.com/tags` claim to session tags, but then every role
  trusted by that provider needs `sts:TagSession` or the call fails
  ([session tags](https://docs.aws.amazon.com/IAM/latest/UserGuide/id_session-tags.html)) *(search)*.
  AWS does not document what STS does when a token's `kid` is missing from its cached JWKS; a
  GitLab issue reports concurrent `AssumeRoleWithWebIdentity` calls failing with "couldn't retrieve
  verification key" and AWS advising a longer `Cache-Control` on the JWKS
  ([gitlab#374001](https://gitlab.com/gitlab-org/gitlab/-/issues/374001)) *(search)*.
- **AWS IAM Roles Anywhere.** A workload with an X.509 certificate from a registered trust anchor
  signs a `CreateSession` request and gets temporary credentials, 3,600 seconds by default
  ([API model](https://github.com/boto/botocore/blob/develop/botocore/data/rolesanywhere/2018-05-10/service-2.json)).
  The credential helper keeps the private key in a TPM, PKCS#11 token or OS keystore, and its
  `serve` command answers like IMDSv2 on localhost; "any process that can reach 127.0.0.1 can
  retrieve AWS credentials from the credential helper"
  ([helper](https://github.com/aws/rolesanywhere-credential-helper)).
- **Google Cloud Workload Identity Federation.** A pool provider trusts an OIDC issuer; STS
  exchanges the token (RFC 8693, `grant_type=urn:ietf:params:oauth:grant-type:token-exchange`) for
  a Google access token. The JWT needs `kid` and `alg` "RS256 or ES256"; `iat` "must be in the past
  and no more than 24 hours in the past"; for `exp`, "we recommend setting an expiration time less
  than 6 hours" ([STS token](https://cloud.google.com/iam/docs/reference/sts/rest/v1/TopLevel/token)).
  The provider fetches the JWKS from `<issuer>/.well-known/openid-configuration`, or the JWKS can
  be **uploaded** (`jwksJson`, EC or RSA keys), so the issuer need not be reachable; `allowedAudiences`
  takes up to 10 values of 256 characters; `google.subject` "cannot exceed 127 bytes"; an
  `attributeCondition` in CEL can test any claim, and "multi-tenant identity providers (such as
  GitHub or Terraform Cloud) require an `attribute_condition` to prevent token spoofing"
  ([provider reference](https://cloud.google.com/iam/docs/reference/rest/v1/projects.locations.workloadIdentityPools.providers),
  [overview](https://cloud.google.com/iam/docs/workload-identity-federation-with-other-providers)).
  STS also takes X.509 client certificates (`urn:ietf:params:oauth:token-type:mtls`).
- **Microsoft Entra workload identity federation.** A federated identity credential on an app
  registration or a user-assigned managed identity names an issuer, subject and audience, which
  "must case-sensitively match" the token's; the token is exchanged with the client-credentials
  flow as a client assertion ([concepts](https://learn.microsoft.com/en-us/entra/workload-id/workload-identity-federation)).
  "Only issuers that provide tokens signed using the RS256 algorithm are supported … Exchanging
  tokens signed with other algorithms may work, but haven't been tested"; changes take minutes to
  propagate ([considerations](https://learn.microsoft.com/en-us/entra/workload-id/workload-identity-federation-considerations)).
  Entra "stores only the first 100 signing keys" it downloads from an issuer. The recommended
  audience is `api://AzureADTokenExchange`
  ([create trust](https://learn.microsoft.com/en-us/entra/workload-id/workload-identity-federation-create-trust)).
  There is a limit of 20 federated credentials per identity; flexible credentials, which match
  `sub` with wildcards, are in preview and only for GitHub, GitLab and Terraform Cloud tokens
  ([flexible credentials](https://learn.microsoft.com/en-us/entra/workload-id/workload-identities-flexible-federated-identity-credentials)).
- **APIs that were key-only now federate too.** OpenAI exchanges "a short-lived token from your
  identity provider" for "a short-lived OpenAI access token"
  ([OpenAI WIF](https://developers.openai.com/api/docs/guides/workload-identity-federation)) *(search)*;
  Anthropic validates a JWT from the workload's issuer against rules in the Console and "returns a
  short-lived Anthropic access token bound to a service account"
  ([Anthropic WIF](https://platform.claude.com/docs/en/manage-claude/workload-identity-federation)) *(search)*.
  Docker Hub trusts GitHub's OIDC tokens through "OIDC connections"
  ([GitHub: OIDC in Docker](https://docs.github.com/en/actions/how-tos/secure-your-work/security-harden-deployments/oidc-in-docker)).
  PyPI's trusted publishing mints an API token "only valid for 15 minutes from time of creation"
  ([trusted publishers](https://docs.pypi.org/trusted-publishers/)). So the audit's "most LLM
  providers issue only static keys" (S2) is out of date for the two the gateway example targets.
- **Vault** accepts JWTs through its JWT auth method (`bound_audiences`, `bound_claims`, a
  `jwks_url` or discovery URL) ([JWT auth](https://developer.hashicorp.com/vault/docs/auth/jwt)).

### 1.3 SPIFFE and SPIRE

- **Identity without a client secret.** "The SPIFFE Workload Endpoint specification mandates the
  absence of direct client authentication, instead relying on out-of-band authenticity checks":
  the agent identifies the calling process itself
  ([Workload API](https://github.com/spiffe/spiffe/blob/main/standards/SPIFFE_Workload_API.md)),
  with attestors for Unix uid/gid, Docker and Kubernetes, after the agent has attested its node
  (AWS, Azure and GCP instance identity, TPM, join token, x509pop)
  ([agent](https://github.com/spiffe/spire/blob/main/doc/spire_agent.md)).
- **Lifetimes.** Defaults: `default_x509_svid_ttl` 1 h, `default_jwt_svid_ttl` 5 m, `ca_ttl`
  (also the JWT signing key's) 24 h, key type ec-p256. "SPIRE schedules activation of the next CA
  or signing key when the current authority has one sixth of its lifetime remaining"
  ([server](https://github.com/spiffe/spire/blob/main/doc/spire_server.md#ca-and-svid-lifetimes)).
  The agent renews a JWT-SVID once it has less than half of its lifetime left
  ([rotationutil](https://github.com/spiffe/spire/blob/main/pkg/common/rotationutil/rotationutil.go)).
- **Keys.** Key managers hold signing keys on disk, in memory ("unpersisted keys in memory"), or in
  AWS KMS, GCP KMS, Azure Key Vault, Vault Transit or KMIP
  ([server plugins](https://github.com/spiffe/spire/blob/main/doc/spire_server.md),
  [memory](https://github.com/spiffe/spire/blob/main/doc/plugin_server_keymanager_memory.md)). An
  OIDC discovery provider publishes the JWKS so AWS, GCP and Entra can verify JWT-SVIDs
  ([provider](https://github.com/spiffe/spire/blob/main/support/oidc-discovery-provider/README.md)).
- **Bearer caveats.** JWT-SVIDs must carry `aud` and `exp`; one audience is "strongly recommended",
  because "tokens sent to one audience can be replayed to another audience should more than one be
  present", and "being a bearer token, JWT-SVIDs are susceptible to replay attacks"
  ([JWT-SVID](https://github.com/spiffe/spiffe/blob/main/standards/JWT-SVID.md)).

### 1.4 Egress that adds the secret, so the app never holds it

- **Fly.io's tokenizer** "is an HTTP proxy that injects third party authentication credentials into
  requests. Clients encrypt third party secrets using the proxy's public key" (a NaCl sealed box)
  and send the sealed blob in `Proxy-Tokenizer`; the blob also says how the client must
  authenticate (a bearer digest, a macaroon, or a signed `Fly-src` header naming the org, app and
  Machine) ([README](https://github.com/superfly/tokenizer),
  [user guide](https://github.com/superfly/tokenizer/blob/main/docs/UserGuide.md)).
  - Processors inject a header (`fmt`, `dst`), a query parameter (`query:<param>`, with a warning
    that query strings reach access logs), an HMAC of the body, an AWS SigV4 re-signature, or run a
    JWT-bearer, client-credentials or GitHub App flow and hand back a *sealed* access token, so
    "the caller never sees any plaintext credential".
  - `allowed_hosts` exists because "if a client is fully compromised, the attacker could send
    encrypted secrets via tokenizer to a service that simply echoes back the request".
  - Clients must speak plain HTTP to it so it can rewrite the request, and it holds one long-lived
    private key (`OPEN_KEY`).
- **Envoy's credential injector** adds a Basic or Bearer `Authorization` header to proxied
  requests, from a generic secret or an OAuth2 client-credentials flow, "to authenticate the
  workload itself" ([docs](https://www.envoyproxy.io/docs/envoy/latest/configuration/http/http_filters/credential_injector_filter)).
- **Cloudflare.** Outbound Workers "sit between your customer's Workers and the public Internet"
  and can "configure authentication to your APIs behind the scenes"
  ([Workers for Platforms](https://developers.cloudflare.com/cloudflare-for-platforms/workers-for-platforms/configuration/outbound-workers/)).
  Since April 2026 Sandboxes and Containers get "zero-trust credential injection": "because
  outbound handlers run in the Workers runtime, outside the sandbox, they can hold secrets the
  sandbox never sees", keyed per instance if needed, "without exposing secrets to untrusted
  workloads" ([changelog](https://github.com/cloudflare/cloudflare-docs/blob/production/src/content/changelog/containers/2026-04-13-sandbox-outbound-workers-tls-auth.mdx)).
  AI Gateway's BYOK stores provider keys "instead of including API keys in every request"
  ([BYOK](https://developers.cloudflare.com/ai-gateway/configuration/bring-your-own-keys/)).

### 1.5 Serverless and Wasm platforms

- **Cloudflare Workers.** Secrets are "encrypted text values" bound to a Worker and read from
  `env` like other variables ([secrets](https://developers.cloudflare.com/workers/configuration/secrets/)).
  Secrets Store (beta) holds account-level secrets; a Worker reads one with
  `await env.<BINDING>.get()` ([Secrets Store](https://developers.cloudflare.com/secrets-store/),
  [Workers](https://developers.cloudflare.com/secrets-store/integrations/workers/)). Either way the
  code holds the value; only the outbound-Worker pattern above keeps it out.
- **Deno Deploy.** Environment variables marked secret are "never visible in the UI after
  creation" ([env vars](https://docs.deno.com/deploy/reference/env_vars_and_contexts/)), and the
  app holds them. For clouds it issues OIDC tokens instead (1.1).
- **Spin.** Variables marked `secret = true` "should be treated as sensitive"
  ([manifest](https://spinframework.dev/v3/manifest-reference)); the component reads the value.
  Values come from providers in the runtime config: environment variables, Vault or OpenBao
  (KV v2, configured with a `token`), or Azure Key Vault ("you must use the client credentials
  flow", configured with a `client_secret`)
  ([dynamic configuration](https://spinframework.dev/v3/dynamic-configuration)). The provider's
  own credential is long-lived.
- **wasmCloud** (v1). "The host retrieves secrets from a secrets backend and provides them to
  components"; backends talk to hosts over NATS, encrypted with per-request x25519 "xkeys"; a
  component calls `get` on `wasmcloud:secrets` and `reveal` on the result
  ([concepts](https://wasmcloud.com/docs/v1/concepts/secrets/)). A request carries the component's
  embedded JWT so the backend can check it
  ([backend API](https://wasmcloud.com/docs/v1/deployment/security/secrets/)). The Vault backend
  is itself an issuer: it signs a JWT from the host's and component's claims, serves the JWKS, and
  logs in to Vault with JWT auth
  ([secrets-vault](https://github.com/wasmCloud/contrib/tree/main/secrets/secrets-vault)). The
  next major release plans "`wasi:config` backed by Kubernetes secrets" instead.
- **Vault dynamic secrets.** "With every dynamic secret … Vault creates a lease"; once it expires
  "Vault can automatically revoke the data" ([leases](https://developer.hashicorp.com/vault/docs/concepts/lease)).
  The database engine creates per-lease credentials, so each service has its own
  ([databases](https://developer.hashicorp.com/vault/docs/secrets/databases)). Vault's own token
  issuer rotates named keys "every 24 hours", keeping each public key for verification for another
  24 hours; algorithms RS256 (default) to ES512 and EdDSA
  ([identity tokens](https://developer.hashicorp.com/vault/docs/secrets/identity/identity-token),
  [API](https://developer.hashicorp.com/vault/api-docs/secret/identity/tokens)). With plugin
  workload identity federation (Enterprise), Vault's AWS engine exchanges a Vault-signed token for
  STS credentials instead of holding static ones
  ([AWS engine](https://developer.hashicorp.com/vault/docs/secrets/aws#plugin-workload-identity-federation-wif)).

### 1.6 Attenuable capability tokens

- **Macaroons** are bearer tokens authenticated by a chain of HMACs: anyone can add a caveat, none
  can be removed, and third-party caveats delegate a check to another service
  ([paper](https://research.google/pubs/macaroons-cookies-with-contextual-caveats-for-decentralized-authorization-in-the-cloud/)).
  Verifying needs the root key, so only the issuer (or whoever shares its key) can check them.
  Fly.io's API tokens are macaroons ([design notes](https://github.com/superfly/macaroon/blob/main/macaroon-thought.md)),
  yet "tokens are valid for 20 years (175200h0m0s) by default"
  ([Fly tokens](https://fly.io/docs/security/tokens/)): attenuable is not the same as short-lived.
- **Biscuits** "can be verified by any system that knows the root public key", carry Datalog
  checks, and can be attenuated offline with ephemeral keys; signatures are Ed25519 or P-256
  ([specification](https://github.com/eclipse-biscuit/biscuit/blob/main/SPECIFICATIONS.md)).
- **For nanovm:** no cloud or API accepts either, so neither helps an app reach an external
  service. They would fit capabilities nanovm both mints and checks (for example an object
  delegating a narrowed right to another app), which nothing needs today. Not recommended now.

### 1.7 APIs that only take a static key

- **Stripe** offers restricted keys with Read, Write or None per resource
  ([restricted keys](https://docs.stripe.com/keys/restricted-api-keys)), and rolling: "Rolling a
  key revokes it and generates a replacement key", with the old key expiring now, in 1 hour, 24
  hours, 3 days or 7 days; "expire it only after its request volume has been at zero for a few
  hours or days" ([API keys](https://docs.stripe.com/keys)) *(search)*.
- **The least-bad pattern**, from 1.4 and 1.5:
  - the app never holds the key: egress adds it (tokenizer, Envoy, Cloudflare outbound Workers);
  - the key is bound to hosts (and a path) by whoever set it, not by the code that uses it
    (tokenizer's sealed `allowed_hosts`), so new code can't send it elsewhere;
  - prefer headers to query parameters (tokenizer's warning);
  - the narrowest key the provider offers (Stripe's restricted keys);
  - rotation by the provider's roll with an overlap, watching the old key's traffic fall to zero;
  - one stored copy, not one per node.

### 1.8 Keeping the issuer's own key short-lived or out of reach

- **Rotate and publish.** Deno Deploy and GitHub publish JWKS and rotate; relying parties must
  "fetch the JWKS keys dynamically" ([Deno](https://docs.deno.com/deploy/reference/oidc/#verifying-tokens)).
  Vault rotates daily and keeps old public keys a day (1.5). SPIRE activates the next key at a sixth
  of the current one's life (1.3).
- **In memory.** SPIRE's `memory` key manager keeps keys only in memory (1.3).
- **In a KMS.** SPIRE's KMS key managers; Sigstore's Fulcio, whose leaf certificates are "valid for
  10 minutes", builds its CA chains with AWS, GCP, Azure or Vault KMS keys
  ([Fulcio](https://github.com/sigstore/fulcio), [security model](https://github.com/sigstore/fulcio/blob/main/docs/security-model.md));
  AWS KMS `Sign` supports `ECC_NIST_P256` keys with `ECDSA_SHA_256`
  ([API model](https://github.com/boto/botocore/blob/develop/botocore/data/kms/2014-11-01/service-2.json)).
  Kubernetes' API server reads its signing key from a file, so "anyone with privileged access to
  the kube-apiserver configuration / filesystem could exfiltrate signing material"; an external
  signer that "never returns signing material" is now stable
  ([KEP-740](https://github.com/kubernetes/enhancements/tree/master/keps/sig-auth/740-service-account-external-signing)).
- **Per-node keys certified by a root.** SPIRE does this for X.509 (a CA chain). AWS, GCP and Entra
  check JWT signatures against the issuer's JWKS, not a certificate chain, so for federation each
  node's key must be in the JWKS itself (Entra keeps the first 100). A root adds nothing there.
- **Several nodes.** Either they share one key (a KMS, or a copied file: a long-lived secret), or
  each signs with its own and the JWKS lists them all, as SPIRE's bundle lists every server's keys.

### 1.9 What follows for nanovm

1. **The platform should be the issuer.** Every platform above gives its workloads an OIDC token
   with a stable, readable subject and lets clouds and APIs exchange it. nanovm already has the
   facts for one: who owns the host, the repository, the digest, the signer.
2. **Attestation by construction.** SPIRE identifies callers out of band; nanovm's supervisor
   already knows which app every egress link belongs to (one socketpair per worker), so the guest
   needs no credential to ask.
3. **Short and narrow.** 5 minutes (GitHub, Deno, SPIRE), one audience (SPIFFE), ES256 or RS256
   (AWS and GCP take both; Entra has tested only RS256).
4. **Subjects must not be reassignable** (GitHub's 2026 change). nanovm's subject is a domain and a
   repository, both of which can change hands: see the threat model.
5. **Per-tenant issuers** (Fly's per org, Vercel's per team) keep a trust policy that forgets its
   `sub` condition from admitting every tenant; GCP says multi-tenant issuers *require* a
   condition.
6. **Static keys belong in egress**, bound to hosts by whoever set them (1.4, 1.7).
7. **The issuer's key** should be in memory and rotated, or in a KMS; never in a file.
8. **Code size.** The pieces are small: a JWT is two base64url JSON objects and a signature, and
   `ring` (already in the tree through `rustls` and `object_store`) signs ES256.

## 2. Recommended design

### 2.1 Overview

```
                          AWS STS · GCP STS · Entra · Vault · OpenAI · Anthropic · own APIs
                                ▲ token (5 min)              │ fetch JWKS
 guest ──GET http://nanovm/token?aud=…──► worker ──egress link──► supervisor (signs, own key)
 guest ──https://api.stripe.com/…───────► worker ──egress link──► supervisor adds the key ──► Stripe
                                                                    ▲ binding + value, cached
                                                    store: secrets/<app>/<name> (developer-signed)
 proxy: polls each node's /_nanovm/keys ──► serves https://<issuer>/jwks.json and discovery
```

### 2.2 Who issues what

| Credential | Issued by | To | Lifetime | Audience | Rotation |
|---|---|---|---|---|---|
| App token (JWT, ES256) | the node's supervisor | the app's guest, or attached by egress | 5 min (`nbf` 1 min early) | one, chosen by the app | per request |
| Node signing key (P-256) | each supervisor, in memory | nobody; public half to the proxy | 24 h | — | next key published 1 h before use; old key published 1 h after |
| JWKS and discovery | the proxy, from its listed nodes | anyone (public) | `Cache-Control: max-age=300` | — | follows the keys |
| Cloud credentials | AWS/GCP/Azure, in exchange for an app token | the guest | 15 min (AWS minimum) to 1 h | the cloud | per exchange |
| Static third-party key | the provider, set by the developer | the supervisor, never the guest | the provider's; rolled by the developer | the bound hosts | developer re-sets; nodes see it within 60 s |
| Node's store credential | the platform (instance role, IRSA, Roles Anywhere, MinIO STS) | the supervisor | 1 h typical, refreshed by `object_store` | the store | automatic |

### 2.3 The app token

```
header  {"alg":"ES256","typ":"JWT","kid":"<base64url of SHA-256(public key), 16 bytes>"}
claims  {
  "iss": "https://id.example.net/example.com",       issuer per host (2.4)
  "sub": "example.com/ghcr.io/org/shop",              the app: owning host + registry/repository
  "aud": "sts.amazonaws.com",                         exactly what the guest asked for
  "iat": 1763100000, "nbf": 1763099940, "exp": 1763100300,
  "jti": "<16 random bytes, base64url>",
  "repository": "ghcr.io/org/shop",
  "digest": "sha256:…", "version": "1.4.0",
  "signer": "https://token.actions.githubusercontent.com https://github.com/org/shop/.github/workflows/publish-app.yml@refs/heads/main",
  "object": "u-ann",
  "node": "10.0.0.5:8080"
}
```

- **`sub` is the app**, exactly as nanovm already defines it for data and uid: the name that owns
  the `_nanovm` record (after CNAMEs, `nanovm-host`) and the repository. It does not change across
  versions, so trust policies don't break on deploy. A slash separates host from repository (a
  host has none). Example: `example.com/ghcr.io/org/shop`, 28 bytes; GCP's `google.subject` caps
  at 127, so very long names map `attribute.app` instead.
- **`signer`** is the `<issuer> <subject>` whose Sigstore signature verified this digest
  (`registry.rs` `verify` would return it instead of `()`). GCP's CEL and Vault's `bound_claims`
  can pin it; AWS can only through session tags, which force `sts:TagSession` on every role, so the
  token carries no tags by default (question 4).
- **`object`** is named by the worker, as for publishes ("the supervisor names the app, the worker
  names the object", README). It is as trustworthy as the app's own code: informational, not for
  trust policies.
- **ES256**: AWS and GCP accept it; Entra has tested only RS256 (question 2). `ring` signs ES256
  and generates P-256 keys; it cannot generate RSA keys, but `aws-lc-rs`, already in the lock file
  through `sigstore-crypto`, can.
- **5 minutes**, single audience, `nbf` one minute early for skew (Deno). An exchange happens right
  after issuance; AWS credentials then last at least 15 minutes, which is the real window.
- **No refresh protocol.** The guest asks again when it needs one; the response says how long it
  is good for.

### 2.4 Keys and rotation across nodes

- **Issuer URL.** `issuer = "https://id.example.net"` in the node and proxy configs (absent: no
  tokens). Each host gets its own issuer, `https://id.example.net/<host>`: an AWS OIDC provider,
  GCP pool provider or Entra credential registered for `example.com` admits only `example.com`'s
  tokens even if its `sub` condition is wrong or missing (Fly and Vercel do the same per tenant).
  The proxy answers `GET /<host>/.well-known/openid-configuration` with that issuer and a shared
  `jwks_uri`, `https://id.example.net/jwks.json`. TLS comes from the terminator in front of the
  proxy, as for apps.
- **Per-node keys, in memory.** Each supervisor generates a P-256 key at start with `ring` and
  never writes it to disk. A key signs for 24 hours. The next key is made and published an hour
  before it starts signing; a retired key stays published for an hour after (tokens last 5
  minutes). A restart loses the key, and that is fine: tokens are short.
- **Publishing.** The node serves its current, next and retired public keys at `/_nanovm/keys`
  (beside `/_nanovm/ready`). The proxy polls every node in its `nodes` list every 10 seconds and
  serves the union as the JWKS with `Cache-Control: max-age=300`. The proxy *dials* the nodes it
  is configured with, so reaching the proxy's private port is not enough to plant a key.
- **A new key is not used until the proxy has fetched it.** A starting node answers `/token` with
  `503` and `Retry-After: 1` until its key has been served to the proxy once (one poll, ≤10 s).
  A relying party that cached the JWKS just before may still not know the key for up to its cache
  time; whether AWS, GCP and Entra refetch on an unknown `kid` is undocumented, so test it
  (question 1).
- **Revoking a node**: remove it from the proxy's `nodes`. Its keys leave the JWKS at the next
  poll, relying parties drop them within their cache time, and its tokens expire within 5
  minutes. Its platform role is revoked separately (2.9).
- **JWKS size.** One key per node most of the day, two around a rotation. Entra keeps only the
  first 100 keys it downloads, which caps a deployment at about 50 nodes for Entra.
- **Several proxies**: each polls every node, so each serves the same JWKS.
- **Single node** (`nanovm serve` behind one proxy): the same. `nanovm run` signs with an
  in-memory key and a local issuer, for tests.

### 2.5 The node checks the host's record before it signs

Today a node trusts `nanovm-host`, `nanovm-app` and `nanovm-trust` from whoever reaches its port
(audit S3). With tokens, that would let anyone on the private network run their own signed code
under any host and mint that host's cloud credentials. So before signing, the supervisor looks up
`_nanovm.<host>` itself (DNS-over-HTTPS, as the proxy does in `proxy.rs` `resolve`, cached for the
record's TTL) and signs only if the record's owner is the host, its `app=` names this repository,
and its `trust=` lists the identity that signed this digest. The tag and version may differ (a
rollout is in progress). Reaching a node then runs only code the host's DNS already authorizes.
Fixing S3 itself (nftables or SPIFFE mTLS, 2.9) is still recommended; this check makes tokens not
depend on it.

### 2.6 How an app gets and uses a token

- **The request.** `GET http://nanovm/token?aud=<audience>` answers `200` with the JWT as
  `application/jwt` and `Cache-Control: max-age=<seconds left − 60>`; `400` for a missing or empty
  audience or one over 256 bytes (GCP's limit); `404` when the node has no `issuer`; `403` when the
  host's record doesn't authorize this app (2.5); `503` while the key is not yet published.
- **Why an ordinary request, not a WIT interface.** Guests already reach host services this way
  (`http://nanovm/objects/{id}/…`, `http://nanovm/publish`), so it needs no new import, no change
  to the import allowlist, and works in Rust and JavaScript at once. It is the shape of GitHub's
  request URL, Fly's `/v1/tokens/oidc` and Deno's `getIdToken(aud)`. `wasi:config` doesn't fit:
  it is fixed per worker and tokens expire.
- **The path.** The worker's egress hook routes `http://nanovm/token` over the egress link to the
  supervisor, as it does `/telemetry/`; the supervisor's per-worker closure (`supervisor.rs`, where
  it serves the egress link) already holds the app, host and digest, so the guest cannot name
  another app. The worker never holds the key. In-process nodes sign in process.
- **SDKs.** `nanovm_sdk::token("sts.amazonaws.com").await?` (Rust); in JavaScript,
  `await (await fetch("http://nanovm/token?aud=sts.amazonaws.com")).text()`. Helpers for the AWS
  and GCP exchanges can live in the SDK, outside the runtime count.
- **Using it.** With AWS: an unsigned `AssumeRoleWithWebIdentity` call (STS in `egress`), then
  SigV4 in the guest. With GCP: `POST https://sts.googleapis.com/v1/token`, then the access token.
  With Entra: the client-credentials flow with the token as `client_assertion`. With OpenAI or
  Anthropic: their exchange endpoints. With the developer's own API: send it as a bearer and verify
  it against the JWKS.
- **Attached by egress, never seen by the guest.** A manifest entry
  `"identity": {"api.example.org": "https://api.example.org"}` makes the supervisor add
  `Authorization: Bearer <token for that audience>` to requests for that host that carry no
  `Authorization` of their own. The token then never enters the guest. (Exchanging at egress, so
  the guest never holds cloud credentials either, is possible later: question 9.)

### 2.7 Static third-party keys: injected at egress

- **The record.** One JSON object per secret, `secrets/<app>/<name>` in the shared store:

  ```json
  {"app": "example.com/ghcr.io/org/shop", "name": "stripe", "version": 3,
   "to": "api.stripe.com/v1/", "header": "authorization", "format": "Bearer {}",
   "value": "rk_live_…"}
  ```

  or `"query": "client_secret"` instead of `header`/`format`, for GitHub's OAuth token endpoint
  (`"to": "github.com/login/oauth/access_token"`). The binding (`to`, `header` or `query`) is part
  of the record, chosen by whoever set the value, so a new version of the app can't send the key
  to another host (tokenizer's `allowed_hosts`).
- **Injection.** The supervisor's forwarder (`egress.rs` `serve_forwarded`, then `send`) already
  terminates TLS for every outbound request. After the egress policy allows the host, a request
  whose host equals the record's and whose path starts with its path gets the header set (if the
  guest set none) or the query parameter replaced. The guest just calls
  `https://api.stripe.com/v1/charges`. Unlike tokenizer, no plain-HTTP trick is needed: the guest's
  request reaches the supervisor before any TLS.
- **Values only in supervisor memory.** The supervisor lists `secrets/<app>/` when the worker
  starts and refreshes every 60 seconds (one LIST, and a GET per changed ETag), so a rotation
  reaches every node within a minute without restarting workers. A request to a bound host while
  the store has been unreachable for 10 minutes is refused rather than sent without its key.
- **Who sets them: the developer, with a keyless signature.** `nanovm secret` writes the record;
  `cosign sign-blob --bundle` signs it with the developer's OIDC identity (a browser sign-in, or
  ambient GitHub Actions OIDC); the record and bundle go to `PUT https://id.example.net/<host>/secrets/<name>`.
  The proxy resolves the host as for any request and forwards to a node, which:
  - verifies the bundle over the record's SHA-256 with the same Sigstore verifier and the same
    `trust=` identities that may deploy the app (and the operator's `signers` allowlist on beta),
    so whoever may deploy code under the host may set its secrets, and no one else;
  - checks the record names this app, its certificate is under 15 minutes old (Fulcio
    certificates live 10), and its `version` is above the stored one (a conditional put on the
    ETag), so an old update can't be replayed to roll a key back;
  - stores the record and its bundle; the bundle is an audit trail, and the signature's
    transparency-log entry (a hash, not the value) is public.
  A `null` value deletes the binding. The operator never handles the value, and no file is copied
  to nodes.
- **What it does not protect.** Anyone who can read the store (root on a node, the store
  credential) reads the values, as they already read every app's data (audit S1, S6). The fix for
  both is encrypting what goes to the store under a KMS key that nodes reach through their
  identity (question 8); AWS KMS encrypts up to 4,096 bytes directly with an encryption context
  ([API model](https://github.com/boto/botocore/blob/develop/botocore/data/kms/2014-11-01/service-2.json)),
  and `object_store`'s public `AwsAuthorizer` signs the call
  ([docs](https://docs.rs/object_store/0.14.0/object_store/aws/struct.AwsAuthorizer.html)).
- **A host that echoes requests** would leak the key back to the guest; bind keys only to the API
  host, as tokenizer advises.
- **Prefer no key at all.** OpenAI and Anthropic federate (1.2), so the gateway example needs no
  key; Stripe, GitHub's OAuth client secret, Telegram and Slack bot tokens still do. Where an
  identity provider accepts a JWT client assertion from a JWKS URL (Entra does), the app token can
  replace an OAuth client secret entirely.
- **No remote store** (one node, `capture.store` a local directory): secret records are refused,
  since they would be plaintext on the node's disk (question 7). `nanovm run` (dev) reads records
  from a local directory.

### 2.8 Where it sits in the code

| Piece | Where | Runtime lines (estimate) |
|---|---|---|
| Key generation, rotation, JWT signing, JWK export (`identity.rs`) | new | 40 |
| `/token` in the supervisor's egress-link closure; claims | `supervisor.rs` | 20 |
| Route `http://nanovm/token` to the supervisor; sign in process for `nanovm run` | `egress.rs` | 6 |
| `/_nanovm/keys` | `supervisor.rs` | 4 |
| `verify` returns the signer that matched | `registry.rs` | 6 |
| DNS check before signing, sharing the proxy's DoH lookup | `proxy.rs`, `supervisor.rs` | 15 |
| Poll nodes' keys; serve discovery and JWKS; `issuer` keys | `proxy.rs`, `config.rs` | 26 |
| **Identity (phase 1)** | | **≈115** |
| `identity` bindings in the manifest; bearer attach | `config.rs`, `egress.rs` | 10 |
| Secret records: list, refresh, inject header or query | `egress.rs`, `supervisor.rs` | 35 |
| **Egress injection (phase 2)** | | **≈45** |
| `PUT …/secrets/<name>`: verify bundle, check, conditional put; proxy route | `supervisor.rs`, `registry.rs`, `proxy.rs` | 40 |
| **Developer-set secrets (phase 3)** | | **≈40** |

### 2.9 The node's own credentials: store, registry, telemetry, proxy

- **Store (leases, capture, secret records; S1).** Use the platform's identity, which
  `object_store` already reads (`lease.rs` passes the environment to `parse_url_opts`): an EC2
  instance role over IMDSv2, `AWS_WEB_IDENTITY_TOKEN_FILE` with `AWS_ROLE_ARN` (EKS IRSA, or MinIO's
  STS through `AWS_ENDPOINT_URL_STS`), ECS or EKS Pod Identity container credentials
  ([AmazonS3Builder](https://docs.rs/object_store/0.14.0/object_store/aws/struct.AmazonS3Builder.html),
  [config keys](https://docs.rs/object_store/0.14.0/object_store/aws/enum.AmazonS3ConfigKey.html)).
  Off-cloud, IAM Roles Anywhere's helper serves IMDS-style credentials from a TPM-held key (1.2).
  One role per node, limited to the bucket, so fencing a lost node revokes only its role. No code.
  GCS or Azure Blob with their own identity: enable `object_store`'s `gcp-base` or `azure-base`
  (a line each, plus a test). R2, B2 and most S3-compatibles issue only static keys: there the
  least bad is a key scoped to one bucket, sealed with systemd's `LoadCredentialEncrypted=` to the
  TPM (audit S2), rotated by the operator.
  - A node could also sign *its own* token (`sub` `node:<name>`) and exchange it at STS, for bare
    metal with no platform identity. It makes store access depend on the proxy serving the JWKS to
    AWS (credentials last an hour, so a short proxy outage is harmless). Not recommended first
    (question 10).
- **Registry.** Pulls stay anonymous (S17). Private packages (B-102), later: the node pulls with the
  *app's* token (aud the registry's STS), so a developer grants pull on their repository to their
  own app's subject and no other deployment of it can pull. Not now.
- **Telemetry (S7).** No change in nanovm: the node sends to a Collector on loopback; the
  Collector authenticates upstream with the host's cloud identity (`sigv4auth`, `googleclientauth`,
  `azureauth` extensions). Vendors that take only an API key keep it in the Collector alone.
- **Proxy ↔ node (S3).** Stays secretless: on one host an nftables `socket cgroupv2` rule; across
  hosts mTLS with short-lived SPIFFE certificates through a sidecar (SPIRE's X.509-SVIDs last an
  hour by default). Not a shared token. With 2.5, app tokens no longer depend on it, but publishes,
  calls and rate limits still do.

### 2.10 Developers publishing

- **Signing** is already keyless (Sigstore through GitHub attestations or `cosign`; 10-minute
  certificates).
- **Pushing** (S4): publish from CI with `GITHUB_TOKEN` (one job, at most 24 hours), as
  `publish-app.yml` does. For ECR, Artifact Registry and ACR, exchange the job's OIDC token for a
  1-12 hour registry token and pass it as `NANOVM_REGISTRY_TOKEN` (nanovm already sends it as a
  Basic login). Docker Hub trusts GitHub's OIDC through OIDC connections (1.2). From a laptop,
  those registries give short tokens from the developer's SSO session; GHCR needs a PAT, so publish
  to GHCR from CI only.
- **Setting secrets** (2.7) needs only the same keyless signature.
- **DNS** (S10): the deploy authority is the DNS account, outside nanovm. Manage records through
  infrastructure-as-code authenticated by OIDC where the DNS provider allows it.

### 2.11 Threat model

| Who or what | What they get | What limits it |
|---|---|---|
| **A compromised app** (a malicious version signed by a trusted identity, or exploited logic) | Tokens for its own app, any audience: whatever the app's trust policies grant. Use of its bound keys at their bound hosts | It can't name another app (the supervisor stamps identity). It can't read or redirect a bound key (bindings live with the value). Trust policies can pin `signer` where the relying party allows (GCP, Vault) |
| **A worker escape** (code execution in the worker process) | The same as a compromised app, plus any `object` claim within the app | The signing key and secret values are in the supervisor, not the worker. Other apps' links are other processes and uids |
| **A compromised node** (root) | Its signing key: tokens for any app until revoked. Store access: every app's data and secret records | Remove it from the proxy's `nodes` (its keys leave the JWKS within a poll plus relying parties' cache); revoke its per-node platform role; tokens die within 5 min; `node` and `kid` identify what it signed. Rotate the third-party keys it could read. KMS-sealed records (question 8) would need its live role, which revocation ends |
| **A compromised proxy** | It can plant a key in the JWKS and route any host to any signed package | As today, the proxy is trusted for routing. 2.5 stops it making *nodes* sign for hosts whose DNS doesn't agree; planting a key it can't be stopped from. Keep the proxy small and unprivileged |
| **An attacker on the private network** (S3) | Today: run any signed code under any host on a node; forge calls and publishes | 2.5: no tokens for hosts whose record doesn't name the repository and signer. The proxy dials nodes for keys, so no key planting. S3's own fix still needed for the rest |
| **A stolen app token** | Its audience's exchange, for at most 5 minutes | One audience; `exp`; never logged by nanovm (telemetry records the path, not the query or bodies); attached tokens never enter the guest. Replay within 5 min at the same audience is inherent to bearer JWTs (JWT-SVID §7.1); DPoP-style binding isn't accepted by the clouds |
| **Stolen exchanged credentials** | The role, until they expire | Ask for 900 s at AWS; least-privilege roles. They are in guest memory only |
| **A stolen static key** | The provider account | Never in the guest, the package or a node's disk; one store copy. Restricted keys; roll with overlap; KMS sealing (question 8) |
| **Replay of a secret update** | Rolling a key back to an old value | `version` above the stored one, conditional put; certificate under 15 min old |
| **DNS hijack** (S10) | The attacker lists their own signer and deploys their code under the host: tokens for its `sub` | Relying parties that pin `signer` (GCP CEL, Vault) refuse; AWS and Entra, which match `sub` only, don't (question 4). DNSSEC and registrar locks; the record is the deploy authority today too |
| **A host or repository changing hands** (expiry, deletion) | The new owner deploys the old, already-signed package under the same host: same `sub` and `signer` | The same is already true of the app's data. Developers should delete trust policies when they give up a domain. There is no immutable ID to add: a domain has none, and nanovm doesn't see registry IDs |
| **Deploying someone else's public package under your own host** | Tokens with *your* host's `sub` and the publisher's `signer` | Their trust policies match `sub` (and the per-host issuer), so these tokens get nothing. Never trust on `signer` alone |
| **Clock skew, key loss on restart** | Refused tokens for a while | `nbf` 1 min early; new keys published before use; the guest retries |

### 2.12 What it replaces (audit items S1-S17)

| Audit item | Outcome |
|---|---|
| S1 store credential | Platform identity, a role per node (2.9). No code for S3-compatibles with STS |
| S2 secrets directory | Removed. Cloud and federating APIs: app tokens (2.3-2.6). Static keys: egress injection from developer-signed records (2.7) |
| S3 proxy↔node trust | nftables or SPIFFE mTLS (2.9); app tokens additionally guarded by the DNS check (2.5) |
| S4 registry push credential | CI's `GITHUB_TOKEN`, or OIDC-exchanged registry tokens; no PAT except GHCR from a laptop (2.10) |
| S5 secrets in public packages | Packages hold no secrets: tokens and injected keys replace them. Keep beta's publish-time refusal of credential-like keys and values (`check_package`) |
| S6 credentials in app data | Unchanged by this design: hash session ids in the oauth example; KMS encryption of shipped versions together with secret records (question 8) |
| S7 telemetry | Loopback Collector with cloud-identity auth (2.9) |
| S8 B-102 pull credentials, B-103 node-sealed secrets | B-102: pulls with the app's token, later (2.9). B-103: replaced by developer-signed records; no long-lived node key (2.7) |
| S9 restic (main) | Removed on beta; on main, `RESTIC_PASSWORD_COMMAND` with the node's identity |
| S10 DNS, signers, Sigstore root | Unchanged; DNS hijack now also reaches tokens unless relying parties pin `signer` (2.11) |
| S11 CI tokens | Unchanged; pin actions by SHA |
| S12 TLS keys | Unchanged |
| S13 containers | Platform identity (IMDSv2 hop limit 2, IRSA, Pod Identity) |
| S14 `nanovm run --secrets` | Replaced by local secret records in dev |
| S15-S17 | Unchanged (not secrets, or already secretless) |

**Code it removes** from `beta/integration`: `secrets.rs` except `check_package` and its two
helpers, the per-second file watcher in `supervisor.rs`, telemetry and error-text masking (the
guest never holds a bound key), secrets in `wasi:config`, the `secrets` directory config and
`nanovm run --secrets`: about 100 of the beta's 132 lines.

### 2.13 Cost

- **Runtime lines** (CI's count): about 115 for identity, 45 for egress injection, 40 for
  developer-set secrets: about 200 over `main` (7,549). Over `beta/integration` (8,711), minus the
  ~100 removed: about +100. Phase 1 alone serves every cloud and federating API.
- **Dependencies:** none new. `ring` (ES256, P-256 key generation, randomness), `base64`,
  `serde_json`, `sha2`, `object_store`, `sigstore-verify` and `reqwest` are already direct
  dependencies or in the tree; `ring` would become a direct one. RS256, if needed, through
  `aws-lc-rs`, already compiled in through `sigstore-crypto` (to confirm with `cargo tree`).
- **Request path:** unchanged for apps that use none of it. A token costs one ECDSA P-256 signature
  in the supervisor (not measured here). A bound host costs a table lookup and a header insert.
  The proxy polls each node every 10 s; each node refreshes secret records once a minute per
  running app that has any; the DNS check is cached for the record's TTL.

### 2.14 Phases

0. **No code:** platform identity for the store (S1, S13); Collector auth extensions (S7); nftables
   or SPIFFE between proxy and nodes (S3); publish from CI (S4); hash the oauth example's session
   ids (S6); pin actions (S11).
1. **Identity:** 2.3-2.6. Tests: a guest's token verifies against the proxy's JWKS and names its
   own app whatever headers it sends; a forged `nanovm-host` whose record doesn't name the
   repository gets `403`; keys rotate and retired keys stay published; a starting node answers 503
   until its key is fetched; a node removed from `nodes` drops out of the JWKS; end to end against
   GCP (uploaded JWKS needs no public issuer) and AWS STS.
2. **Egress injection:** 2.6's attach and 2.7's records, set by the operator through the admin
   socket until phase 3. Tests: the key reaches the bound host and path only; the guest never sees
   it (the `/leak` test component from beta); a changed record is used within 60 s.
3. **Developer-set secrets:** the signed `PUT`. Tests: an identity not in `trust=` is refused; an
   older version is refused; an expired certificate is refused.

## 3. Open questions for the owner

1. **Where does the signing key live?** Per-node keys in memory, rotated daily, published by the
   proxy polling its nodes (2.4); or one key in a KMS that nodes sign with through their identity
   (no rotation gaps, but a cloud KMS in the path of every token and a dependency nanovm can't
   assume); or keys persisted on disk (a long-lived secret again). **Recommended: in memory, per
   node.** First test how long AWS STS, GCP STS and Entra take to accept a new `kid` after a node
   restart; if one caches for long without refetching, offer the KMS option for that deployment.
2. **ES256 only, or RS256 too?** AWS and GCP take ES256; Entra has tested only RS256.
   **Recommended: ES256 only, and test Entra.** If Entra refuses it, add RS256 keys through
   `aws-lc-rs` (about 10 lines).
3. **One issuer per host, or one for the deployment?** **Recommended: per host**
   (`https://<issuer>/<host>`): a trust policy that forgets its `sub` condition then admits only
   that host. It costs nothing in code; relying parties register one provider per host.
4. **Should `sub` include the signer?** AWS and Entra match `sub` only, so with `sub` =
   host/repository they can't refuse code that a DNS hijacker signed. Folding the signer in
   (`example.com/ghcr.io/org/shop@<signer>`) closes that, but breaks trust policies whenever the
   workflow file, branch or signer changes, and can exceed GCP's 127-byte subject. **Recommended:
   keep `sub` = host/repository with `signer` as a claim**; revisit if users ask, perhaps as a
   per-record option.
5. **Is the DNS check before signing (2.5) worth 15 lines?** It makes tokens independent of
   proxy-node trust (S3), which is network position today. **Recommended: yes.** Without it, an
   issuer must not be enabled until S3 is fixed in the deployment.
6. **Static keys: developer-set signed records in the store (2.7), operator-set records, or a
   secret manager the node reads with the app's identity?** Developer-set means the operator never
   handles a tenant's key and needs no channel to developers; operator-set (through the admin
   socket) is phase 2's stopgap. Reading the developer's own Vault, AWS or GCP secret manager
   keeps the value off the operator's store, but needs code per provider, and the operator sees
   every outbound request anyway (the supervisor terminates TLS), so it protects nothing from the
   operator. **Recommended: developer-set signed records; operator-set only until phase 3 lands.**
7. **Secret records without a remote store?** **Recommended: refuse them** (they would be
   plaintext on the node); single-node deployments that need static keys use an object store, or
   wait for KMS sealing.
8. **Encrypt secret records (and shipped versions, S1/S6) under a KMS key now?** It makes a leaked
   store credential or backup useless, at about 20-30 lines for AWS KMS through `object_store`'s
   `AwsAuthorizer`, plus a KMS the deployment must have. **Recommended: later, once, for records
   and versions together**, as its own change.
9. **Exchange at egress?** The supervisor could trade the app token at GCP STS (RFC 8693) or an
   API's exchange endpoint and attach the access token, so the guest never holds cloud
   credentials either. About 30 lines per protocol. **Recommended: not in v1**; SDK helpers do the
   exchange in the guest. Revisit for GCP first if apps use it.
10. **Should a node exchange its own token for store credentials?** It would serve bare metal
    with no platform identity, at about 10 lines, but ties store access to the proxy serving the
    JWKS. **Recommended: no for now**; platform identity, Roles Anywhere or SPIRE first.
11. **Drop the beta's secrets directory?** **Recommended: yes**, with masking and `wasi:config`
    secrets; keep `check_package`'s refusal of credential-like values at publish.
12. **Authenticate calls between apps?** The node could verify an incoming app token (issued by its
    own deployment) and pass the caller's `sub` to the callee in a header, as `nanovm-callers` does
    within an app. **Recommended: later**; apps can verify tokens against the JWKS themselves now.
