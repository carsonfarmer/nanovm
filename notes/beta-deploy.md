# Beta: who deploys, where apps reach, and what an outage costs

Branch `beta/deploy`, from `cf/round3` (with the no-compat cleanup). Issues
B-400 to B-407 in [ISSUES.md](../ISSUES.md).

## The gaps

From the production audit:

1. **Anyone who pointed DNS at the proxy deployed.** The `_nanovm` TXT record
   named the app and the identities it trusted, so the domain owner, not the
   operator, decided what ran on the node (B-400).
2. **The app alone chose its egress.** A signed manifest asking for `*.com`
   reached every `.com` host from the operator's IP (B-401).
3. **Made-up hosts cost a lookup each and wiped the route cache.** Past
   10,000 hosts the cache dropped everything, live routes included (B-402).
4. **CNAME aliasing.** Any domain could CNAME onto another's record and serve
   that app, with its data, under its own origin (B-403).
5. **Outages.** A resolver SERVFAIL read as "no record" (B-404), and once a
   route or digest ran out, a failed lookup took the app down (B-405).

## What changed

**Operator allowlists (proxy).** `hosts` (`name` or `*.name`) and `signers`
(`<issuer> <subject>`, the subject exact or up to a final `*`) in
`proxy.toml`, both empty by default, so a new proxy serves nothing.
- The host check comes first in `forward`, before the cache and before any
  DNS lookup. Calls between objects (the private listener) resolve the app's
  owner name and skip it: the host was admitted when the app was first
  routed.
- A record's `trust=` entries are cut to the ones a signer matches, when the
  record is looked up (so once per TTL, not per request). Only those go to
  the node in `nanovm-trust`, and the node verifies against exactly that
  header, so an app signed by an identity the operator doesn't list is
  refused even when its record trusts it. A record left with none is 403
  without asking the registry for a digest.
- Signer patterns: the whole `<issuer> <subject>` string, with a final `*`
  matching any rest. Validation requires exactly one space and a `*` only at
  the end, so the issuer is always matched exactly.

**Egress cap (node).** The node's `limits.egress` (already "the most an app
gets", but its egress was "the app's own") is the operator's list: a request
must match both lists (`EgressPolicy::matches` checks the operator's first).
Both lists refuse private addresses outside dev mode, and the node refuses to
start with a bad one. Intersecting at match time rather than computing a
list intersection keeps it obviously right (`*.com` ∩ `api.example.com:443`
needs no pattern algebra). `nanovm run` takes the manifest's list as its cap:
its developer is its operator.

**Route cache.** Hosts and tags with an answer (an app, a digest) are kept
apart (up to 10,000) from the rest (up to 1,000): no record (kept 30 s),
refused, failed (5 s), or being looked up. A new key goes to the rest and
moves over once found. When the rest is full of fresh entries, a host the
proxy doesn't know gets 503 instead of a lookup. So made-up names under a
`*.` entry cost at most 1,000 lookups at a time, and can't push out a host
that has an app. Negative answers (no record, refusals) are cached as
before, now as answers of their own (404/403) rather than failures.

Library search: no LRU crate is in the tree (`Cargo.lock` has `indexmap`,
`hashbrown`, `lru-slab`, none an LRU cache). A plain LRU (`lru`) would not
do: 10,000 made-up names evict 10,000 live routes. Scan-resistant caches
(moka's TinyLFU, quick_cache's CLOCK-Pro) would keep hot routes, but neither
bounds the lookups a flood costs, which needs the "unknown" budget anyway;
with that budget, the found side only needs a size cap. So the existing
40-line single-flight cache was reshaped rather than replaced.

**CNAME aliasing: the rule.** An alias is served only if the owner's record
lists it: `_nanovm.example.com TXT "app=…; trust=…; alias=www.example.com"`.
- Why not "the record must be at the requested name": that would undo W-15,
  an owner decision that aliases share one app. With consent in the owner's
  record, sharing stays where the owner of the data asks for it.
- Why not "the app (its signed manifest) lists its hosts": the data belongs
  to the name, not to the signer. A developer signing for many customers
  can't know their hosts, and a signer listing hosts would let one customer's
  build pull another customer's host in.
- Why not "follow CNAMEs but key the app by the requested host": safe (an
  attacker gets the victim's code with data of its own, as copying the record
  already allows), but it silently splits `www` and apex into two apps with
  separate data, which W-15 was made to stop.
- The check is in the lookup, so cached: no per-request cost.

**Resolver status.** Only NOERROR (0) and NXDOMAIN (3) are answers; SERVFAIL
and the rest are failures (502, and stale-on-error below).

**Stale-on-error** (second commit). A found answer stands in for a failed
lookup of it, or one that takes over 5 s, for up to a day past its TTL,
asked for again every 30 s: RFC 8767's serve-stale, with its retry timer
(30 s) and the low end of its maximum stale age (1–3 days). The 5 s wait is
longer than RFC 8767's 1.8 s client timer because a refresh that times out
is cancelled here rather than finished in the background, and a slow but
working registry must not keep an old digest. A host or tag never found
fails as before. The stand-in lives in the slot with the lookup, so a waiter
that takes over a cancelled lookup still has it. Not covered: a proxy
restarted during an outage (the cache is memory only), and the node's own
re-fetch of manifests and signatures when a worker starts (B-406).

## Tests

All new, each a hostile case for one refusal or bound:

| Test | What it shows |
|---|---|
| `unlisted_hosts_are_refused_before_any_lookup` | 7 unlisted spellings and, with no list, every host: 403 and zero DoH queries; listed hosts reach DNS once each |
| `records_trust_only_the_operators_signers` | trust cut to `https://ci repo:org/*` (not `repo:org`, `repo:other/…`, `https://ci.evil`, `https://evil`); none left: 403; empty signers: 403 |
| `only_the_operators_signers_deploy` | real signed counter on ghcr.io, record trusting its CI identity and another: operator lists only the other → node refuses (`not signed by a trusted identity`); operator lists `…/carsonfarmer/*` → 200 |
| `aliases_need_the_owners_consent` | listed alias served as the owner; unlisted CNAME (and a record with no aliases) 403 |
| `made_up_names_cost_bounded_lookups_and_keep_live_routes` | 30,000 made-up names under `*.apps.test`: ≤ 1,001 DoH queries, ≥ 29,000 refused 503, the live route not looked up again |
| `a_failing_resolver_is_not_an_answer` | SERVFAIL → 502, NXDOMAIN → 404 |
| `found_answers_outlast_an_outage_for_a_while` | stale answer on failure and on a hung lookup (after 5 s), not asked again within 30 s, never-found keys fail, stale ends after `stale` |
| `a_resolver_outage_keeps_the_last_route` | resolver answering 503 after the TTL: the route stays, a new host is 502 |
| `the_operators_list_caps_the_apps` | app `*.com` + operator `api.example.com:443` etc.: only the intersection; no operator list: nothing; refused before any lookup |
| `egress_is_capped_by_the_operator` | app asks for every port of a host, operator lists one: that port only, in process and through the jailed supervisor |
| `proxy_lists_are_empty_by_default_and_checked` | empty defaults; malformed `hosts` and `signers` entries refused |
| `the_proxy_routes_hosts_by_dns` (extended) | the three 403s end to end through the node |

Test helpers now configure what they use: test nodes' operator allows the
egress the test apps ask for, test proxies list their hosts and `dev dev`.

**Mutation checks.** Each breaks one mechanism in the source, runs the
tests named, and restores the file with `git checkout --`; `git diff` was
empty of them afterwards. Every one fails at least one test:

| Mutation | Fails |
|---|---|
| M1 the `hosts` check skipped | `unlisted_hosts_are_refused_before_any_lookup` |
| M2 trust not cut to `signers` | `records_trust_only_the_operators_signers` |
| M3 aliases not checked | `aliases_need_the_owners_consent` |
| M4 a full "unknown" side evicted (cleared) instead of refusing | `made_up_names_cost_bounded_lookups_and_keep_live_routes` |
| M5 found answers left with the rest | `caches_are_single_flight_and_bounded` |
| M6 any DNS status taken as an answer | `a_failing_resolver_is_not_an_answer` |
| M7 the operator's egress list ignored | `the_operators_list_caps_the_apps` |
| M8 `hosts` entries unchecked | `proxy_lists_are_empty_by_default_and_checked` |
| M9 no stale answer | `a_resolver_outage_keeps_the_last_route`, `found_answers_outlast_an_outage_for_a_while` |
| M10 no wait bound on a refresh | `found_answers_outlast_an_outage_for_a_while` (times out on the hung lookup) |
| M11 stale forever | `found_answers_outlast_an_outage_for_a_while` |
| M12 exact signers matched as prefixes | `records_trust_only_the_operators_signers` |

The end-to-end tests fail without their mechanism too: with M2 the node
would hear the CI identity in `only_the_operators_signers_deploy` (the
record trusts it), and `egress_is_capped_by_the_operator` relies on M7's
check (its in-process and supervised policies are built by the same
`EgressPolicy::parse`).

**Suite** (`RUST_TEST_THREADS=1 cargo test --locked -p nanovm --no-fail-fast`)
on the branch merged with `cf/round3` 7b3f9c6 (3b0c4c5): lib 164, dev 9,
examples 27, integration 100 (14 ignored, opt-in), js 17, committed_state 2:
all pass, including the signed-app tests against ghcr.io. The example and
JavaScript components were built from the merged sources with
`tools/build-app.sh`. `cargo check --locked -p nanovm --bin nanovm` (no dev
feature) is clean.

## CPU per request

Method: dev-profile builds (dependencies at opt-level 3, nanovm at 1; the
worktree Cargo config) of the base (`9784334`) and this branch (`185f832`),
one jailed node (branch build) behind each proxy, a loopback dev registry,
a DoH stub, `oha` at 64 connections, 50,000 requests per run to 100 counter
objects at one host, after 20,000 warm-up requests per proxy. CPU is the
proxy process's user+system time from `/proc/<pid>/task/*/stat`, divided by
requests. Base and branch alternate, order reversed every other round, 10
rounds each. The machine was shared with five other agents' builds (load
21–30 on 4 cores), so the spread is wide. Scripts: `start.sh`, `ab.sh`,
`measure.py`, `flood.py`, `doh.py` in the session scratchpad (`b-deploy/bench`).

| Proxy, one live host | base | branch |
|---|---|---|
| CPU per request, median (range) | 42.8 µs (39.6–53.0) | 39.9 µs (31.2–45.0) |
| requests/s, median | 5,837 | 6,186 |

No measurable cost: on the hot path the branch adds only the `hosts` scan
(one entry here) before the cache lookup; signers and aliases are checked
once per lookup, not per request.

A flood of 20,000 made-up hosts, 16 keep-alive connections, against each
proxy (the DoH stub answers NXDOMAIN for every name but the live host):

| | base | branch, `hosts = ["*.test"]` | branch, hosts outside the list |
|---|---|---|---|
| answers | 20,000 × 404 | 1,000 × 404, 19,000 × 503 | 20,000 × 403 |
| DoH lookups | 20,000 | 1,000 | 0 |
| live host looked up again afterwards | yes (cache wiped) | no | no |
| proxy CPU per request | 138.0 µs | 37.5 µs | 27.5 µs |

## Runtime lines

CI's count: 7,410 at the base; 7,470 after d287d22 (+60: proxy +42, config
+12, egress +5, main +1); 7,490 after 185f832 (+20, proxy; its commit
message says +15, miscounted); 7,491 after 14c97f9 (+1, the publish hint).
In all +81: 7,499 merged, against 7,418 on `cf/round3` 7b3f9c6.

## Not done

- **B-406:** the node fetches a manifest and its signatures whenever an app's
  worker starts, so a registry outage stops an app whose worker restarts,
  whatever the proxy serves. A fix changes the registry's trust rule
  (signatures are the registry's current answer, by design), so it is the
  owner's call; ISSUES.md has a proposal.
- **B-407:** Caddy still lists the hosts itself; an `ask` endpoint on the
  proxy's private listener would make `hosts` the one list.
- A proxy restarted during an outage has no stale routes to serve (memory
  only).
- The limits agent owns rate limits at the accept loop and request entry
  (since removed: HANDOFF Q91); the 503 for unknown hosts here bounds DoH
  lookups, not requests.

## Owner questions

1. **Empty `hosts` and `signers` by default (a new proxy serves nothing).**
   Recommended: keep. The alternative, "serve anything until listed", is
   the gap itself, and an explicit `*` would make the open mode one line
   away. A beta with a few trusted developers lists them.
2. **Empty node `limits.egress` by default (no app reaches anything).**
   Recommended: keep, for the same reason: the operator's IP is theirs to
   lend. The demo lists `example.com`.
3. **Aliases need the owner's `alias=` (not: records only at the requested
   name; not: hosts listed in the signed manifest).** Recommended: this
   rule, which keeps W-15's shared app with consent from whoever owns the
   data.
4. **Serve stale for up to a day.** RFC 8767 suggests 1–3 days. A shorter
   window takes apps down sooner in a long registry or resolver outage; a
   longer one keeps an old version longer when a tag moved during one.
   Recommended: 1 day.
5. **B-406 (node side of a registry outage).** Recommended: cache the
   manifest and accepted `(digest, policy)` pairs on disk and use them when
   the registry fails (not when it answers "unsigned"), for the same day.
