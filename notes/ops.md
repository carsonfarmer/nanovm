# Ops notes

Operations and packaging work from IDEAS.md (sections E, F, L, M), one branch per
item, each from `cf/main-next`. Plain language; numbers and how to reproduce.

## L-116 release process (`ideas/release-process`)

**What:** `.github/workflows/release.yml`, triggered only by pushed `v*` tags:
builds release binaries (no `dev` feature) for x86_64 and aarch64 Linux, checks
the tag equals the crate version, packages each with `tools/release/package.sh`,
verifies the per-archive checksums, writes `SHA256SUMS`, attests build provenance
and creates the GitHub release (prerelease for `-` tags) with notes from
`tools/release/changelog.py`. `tools/release/README.md` is the process.

**Evidence:** changelog tests 4/4 pass on a throwaway repository; a preview over
`origin/main..cf/main-next` lists the 13 commits with their N- ids. `package.sh`
on a local binary: archive verifies, two runs produce identical bytes, a
non-nanovm binary and a non-version tag are refused; shellcheck clean. The
release job's checksum and prerelease logic were run by hand. The workflow has
not run (needs a tag; no actionlint here); pushing this branch triggers only CI.
