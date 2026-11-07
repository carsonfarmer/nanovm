# Batch 8 — local lint and load controls

## NH16: snapshot-pinned deployment TOML prototype

```sh
python3 -B experiments/nice-to-haves/config_lint.py node demo/nanovm.toml --dev-build --source crates/nanovm/src/config.rs
```

Python 3.11+ `tomllib` parses node/proxy TOML. The schema was read from
`crates/nanovm/src/config.rs`, SHA-256
`4726c8b3f7039a04d1933fe0768ba5e35d1fb43a8b7449fb521cb7d0f7f74d38`.
The CLI refuses any different source bytes until the prototype's schema is reviewed.
This deliberate pin includes comments; it is not a source-code parser. Unit tests
use independent fixtures and do not require the runtime source to remain unchanged.

Unknown/nested keys, types, unsigned ranges, socket syntax and dev/loopback rules
are checked. `usize` is modeled as 64-bit (the tested Linux x64 target). Negative
or nonfinite worker CPU values are prototype policy errors, stricter than a plain
floating-point deserializer. Zero numeric limits retain their meanings; the tool
does not invent positive minima. `--dev-build` is an explicit caller assertion,
not binary inspection. Node/public-control ingress and absent cgroup configuration
are **warnings**, not proof of insecure deployment. No filesystem/data-root checks,
network probes, cgroup writes, host startup or deployment are performed.

Exit 0 = no lint errors (warnings can remain), 1 = errors, 2 = invalid TOML, missing
evidence or source drift. Values are not printed; field names and codes are. Errors
are selected checks, not all runtime validation or production approval. Three tests
cover nested/types/overflow, IPv6/dev policy, warning separation, source drift and
a secret-value canary. The real demo config correctly fails without `--dev-build`.

## NH34: parser-backed Markdown references and headings

```sh
npm install --prefix /tmp/nanovm-nice-tools/markdown --ignore-scripts --no-audit --no-fund unified@11.0.5 remark-parse@11.0.0 github-slugger@2.0.0 mdast-util-to-string@4.0.0
NICE_MARKDOWN_TOOLS=/tmp/nanovm-nice-tools/markdown node experiments/nice-to-haves/markdown_check.mjs . experiments/nice-to-haves/*.md
```

This optional Node tool complements, rather than silently changes, batch 1's
stdlib link checker. It uses pinned CommonMark AST parsing and github-slugger for
heading IDs. It checks reference/inline/image links, nested parentheses, percent
decoding, duplicate heading suffixes, formatted/Unicode headings and static symlink
containment. The first reference definition wins. Inline/fenced code is not links.

No external HTTP requests. HTML/custom anchors, autolink extensions, MDX and GitHub
renderer-specific HTML behavior are not validated; unsupported HTML and non-Markdown
fragments are counted. A link to an HTML-only anchor can report missing heading:
manually review that limitation, not an automatic rewrite. Directory targets are
existence checks, not rendered index resolution. Undefined reference-looking prose
is plain text under CommonMark, not reported as a broken link. No full GitHub
rendering equivalence is claimed. Exit 0/1/2 = no detected problems/problems/tool
unavailable. Two real-filesystem tests exercise the grammar and failure boundaries.
Optional tests skip unless `NICE_MARKDOWN_TOOLS` names the isolated npm prefix.

## NH13: configurable but strictly local load exercise

```sh
python3 -B experiments/nice-to-haves/load_exercise.py experiments/nice-to-haves/fixtures/load-plan.json --concurrency 3 --rate 100 --service-ms 80 --fail-every 3
```

The GET-only driver accepts **no target URL**: it creates and cleans up its own
loopback server. A batch-5 popularity plan supplies object sequence/skew. Limits
are 200 total requests, 8 client workers, 1–200 admissions/s and 0–200 ms synthetic
service delay. Rate means spacing between client admission timestamps, not a
packet-level traffic guarantee. There is no burst catchup; saturation lowers
offered rate. This is a concurrency-limited exercise, not a statistically correct
open-loop latency benchmark (coordinated omission is not corrected).

The fixture checks exact request/object counts, every response's object body, error
oracle and peak simultaneous synthetic service. Service accounting ends before
response delivery, excluding handler teardown. HTTP 503 is an error even when the
transport succeeds. The 12-request all-hot-object fixture expects four errors at
`fail_every=3`; a separate single-request case has null start spacing. The test
checks the concurrency cap and ≥10 ms admission spacing. This does not exercise
nanovm throughput, external load generators, deployment isolation or admission rules.
