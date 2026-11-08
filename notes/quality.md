# Quality notes (quality agent, round 3)

Test and verification work from IDEAS.md sections D and G. Each item is on its own branch from
`cf/main-next`; this file collects the notes. Commands run from the repository root.

## G-72: supply-chain checks (`ideas/supply-chain`)

**What it adds.** `deny.toml` (cargo-deny 0.20.2: advisories, licenses, bans, sources),
`.cargo/audit.toml` (cargo-audit 0.22.2) and `.github/workflows/supply-chain.yml`, which runs both
on pushes and pull requests that touch a manifest, lockfile or either config, daily (new advisories
appear without any change here) and on demand. The node's workspace gets every cargo-deny check;
each other lockfile (guests, KV and SQLite components, the post-return fixture, the SQLite
experiment) gets advisories and sources, since their own crates are unlicensed path crates.

**Policy.** Linux targets only (x86_64, aarch64). Licenses allowed: the permissive set the graph
actually uses (MIT, Apache-2.0 with or without LLVM-exception, BSD-2/3, ISC, Zlib, Unicode-3.0,
CDLA-Permissive-2.0 for webpki roots, BSL-1.0, CC0, MIT-0, Unlicense); no copyleft is needed (`r-efi`
offers LGPL only as one alternative of three). Only crates.io as a source; no git dependencies;
wildcards denied; duplicate versions warn (13 crates, all inside wasmtime/sigstore/rand
ecosystems: not worth fighting).

**What they flagged at `cf/main-next` (0ac2849), and what was done.**

| Finding | Where from | Action |
| --- | --- | --- |
| RUSTSEC-2023-0018 (vulnerability): `remove_dir_all` 0.5.3 TOCTOU | `tempdir` 0.3.7 ← `container-registry` 0.3.1 `test-support` (dev-dependency, the tests' loopback registry) | Ignored with reason in both configs. Not in the node binary; `container-registry` 0.3.1 is the latest and requires `tempdir` for `test-support`, and `tempdir` pins `remove_dir_all` ^0.5, so no upgrade exists. The tests delete only their own temporary storage. (N-501) |
| RUSTSEC-2018-0017 (unmaintained): `tempdir` | same path | Same. |
| `nanovm` is unlicensed | the workspace crate has no `license` | `publish = false` in `crates/nanovm/Cargo.toml` (true: it isn't published), so cargo-deny's private-crate rule skips it. No license was invented. |
| `experiments/fs-backends/Cargo.lock` is stale | `--locked` refuses it | Skipped in CI, noted in N-501. |

No dependency was upgraded or changed; `Cargo.lock` is untouched. After the changes:
`cargo deny --locked check` → `advisories ok, bans ok, licenses ok, sources ok`; `cargo audit`
→ exit 0 on all six lockfiles (416 crates in the node's). Without `.cargo/audit.toml`, `cargo audit`
reports both advisories and exits 1, so the exceptions are what keeps it green.

Run: `cargo install --locked cargo-deny --version 0.20.2 && cargo install --locked cargo-audit
--version 0.22.2`, then `cargo deny --locked check` and `cargo audit`.
