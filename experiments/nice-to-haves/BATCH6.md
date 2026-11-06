# Batch 6 — offline review evidence

`review_tools.py` adds NH41/NH45/NH46 and the offline integrity portion of NH37.
It reads trusted local inputs; it does not fetch registries, verify signatures,
publish packages, grant license-policy approval or generate release notes without
human review. No runtime/config/guest/root dependency edits.

```sh
# Hash and compare existing binaries, without rebuilding them.
python3 -B experiments/nice-to-haves/review_tools.py size target/keyvalue/tests.wasm target/keyvalue/tests.wasm --before-toolchain existing-unverified --after-toolchain existing-unverified --budget 443534
# Metadata is declarations, not necessarily the packages linked into a deployed binary.
cargo metadata --locked --offline --format-version 1 > /tmp/nice-cargo-metadata.json
python3 -B experiments/nice-to-haves/review_tools.py licenses /tmp/nice-cargo-metadata.json
# Explicit full commits, no implicit HEAD/tag/fetch/release.
python3 -B experiments/nice-to-haves/review_tools.py changelog . "$(git rev-parse 49de2ff)" "$(git rev-parse 2e82381)"
# A local OCI image manifest and a blob directory containing sha256/<hex> files.
python3 -B experiments/nice-to-haves/review_tools.py oci /path/to/manifest.json /path/to/blobs
```

Size reports retain both digests/toolchain labels and top-level section lists.
Toolchain strings are caller claims, not authenticated build provenance. Budget
equality passes; reductions are negative deltas; a changed toolchain is explicitly
flagged rather than silently compared as equivalent. Successful analysis exits 0
even over budget: inspect `over_budget`. Byte size is not startup/runtime speed.

License review retains package name/version, declared SPDX expression verbatim,
source **kind**, and whether a license file exists in metadata. It never reads the
license file or prints its path, repository/source URL, environment or package ID.
Expressions are not parsed/evaluated; `OR` is not replaced with `AND`. Distinct
sources with the same name/version can yield repeated rows; this worksheet is not
a deduplicated SBOM. Missing declarations are review findings, not legal verdicts.
Free-form package/license text can itself be private: inspect before sharing.

Changelog output includes base-exclusive/head-inclusive commit subjects and IDs,
including merges; the base must be an ancestor and both endpoints commit objects.
Subjects are NUL-delimited so Unicode line separators remain intact. Reverts and
breaking changes are not interpreted. Private subjects still need human review.

OCI inventory supports only schema-v2 OCI image manifests and lowercase SHA-256
descriptors, at most 128 layers, 4 MiB manifest and 64 MiB per blob. It checks each
descriptor's local byte length/hash without following URL fields. Static symlinks
outside the blob root fail. These are trusted local files: no hostile concurrent
filesystem-mutation or special-file protection is claimed. Annotations are omitted.
The result always says `signature_status: NOT VERIFIED`; hashes show integrity
against the supplied manifest, not authenticity. No online registry interface,
Sigstore verifier, deployment policy or host compatibility check was added.

Four tests pass: byte-budget boundaries/reductions/digests, license-expression
preservation with secret path/URL canaries, real temporary Git range/ancestry cases,
and real temporary OCI blobs with digest/size/missing-file/symlink failures. The
Unicode separator test first failed on newline splitting, then passed with NUL
framing. Real local Cargo metadata yielded 448 package declarations and one missing
declaration; no license policy was applied or metadata changed. The existing
443,534-byte component compared with itself gives zero delta and passes equality.
The explicit local commit range produced three review candidates, no release.
