# Releasing nanovm

A version tag `vX.Y.Z` (or `vX.Y.Z-rc.N`, published as a prerelease) runs
[.github/workflows/release.yml](../../.github/workflows/release.yml). Nothing else
does: pushing a branch runs no release job.

1. Land the version bump on `main` like any change, with CI green: `version` in
   `crates/nanovm/Cargo.toml` (and the `nanovm` entry in `Cargo.lock`, which
   `cargo check` updates). The workflow refuses a tag that doesn't match it.
2. Preview the notes: `tools/release/changelog.py HEAD` (commits since the last
   `v*` tag). Call out by hand anything an operator must act on: config keys,
   journal format (a newer journal tag cannot be recovered by an older binary;
   see README "Commits", Downgrades), host requirements.
3. Tag the commit CI passed on and push the tag:
   ```sh
   git tag -a v0.2.0 -m "nanovm v0.2.0" <commit> && git push origin v0.2.0
   ```
4. The workflow builds `nanovm` in release mode without the `dev` feature for
   `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`, packages each with
   [package.sh](package.sh) (binary, README, `deploy/`, a `.sha256`), checks the
   checksums, writes `SHA256SUMS`, attests build provenance (as the guests are),
   and creates the GitHub release with [changelog.py](changelog.py)'s notes.
5. Check what was published:
   ```sh
   gh release download v0.2.0 -p 'nanovm-*' -p SHA256SUMS && sha256sum -c SHA256SUMS
   gh attestation verify nanovm-v0.2.0-x86_64-unknown-linux-gnu.tar.gz --repo carsonfarmer/nanovm
   ```

Deploying a release is separate: see the upgrade and rollback runbooks.

`package.sh` makes the same archive locally, byte for byte for the same binary
and commit (fixed order, owner and mtime): `tools/release/package.sh
target/release/nanovm v0.2.0 x86_64-unknown-linux-gnu dist`. The
`ubuntu-24.04-arm` runner is free for public repositories only; drop that matrix
row otherwise.

## Evidence

- `python3 -m unittest discover -s tools/release -v`: 4 tests against a throwaway
  repository (previous-tag detection, merges left out, first release, preview
  ranges, non-version tags ignored).
- `package.sh` run locally on a nanovm binary: archive and `.sha256` verify, two
  runs give identical archives, a non-nanovm binary and a non-version tag are
  refused. The release job's checksum and prerelease steps were run by hand.
- The workflow itself has not run: it needs a pushed tag. Its YAML parses; there
  was no `actionlint` here.

The nice-to-haves branch's `review_tools.py changelog` produces JSON review
candidates for an explicit SHA range; this script writes the Markdown notes a
release publishes.
