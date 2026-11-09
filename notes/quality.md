# Quality notes (quality agent, round 3)

Test and verification work from IDEAS.md sections D and G. Each item is on its own branch from
`cf/main-next`; this file collects the notes. Commands run from the repository root.

## D-46: model-based quota test (`ideas/quota-model`)

**What it is.** `crates/nanovm/tests/quota_model.rs` (proptest) drives a test-only component,
`crates/nanovm/tests/fsops` (outside `guests/**`), on an in-process node with a 1 MiB / 4-entry
quota. Each case is a fresh object and up to 19 requests of 1-5 random operations on `/data`
(write at an offset, including sparse and over-budget writes; truncate; rename, including over
files and directories and across directories; unlink; mkdir; rmdir) over nine overlapping paths.
A request commits (75 %) or rolls back. A model of the volume predicts every result (`ok`,
`insufficient-space`, `quota` or another error) from POSIX semantics plus the quota's rules: the
entry check before the parent check, short writes cut at the budget, holes charged, truncation
charged and credited, renames over files freeing them, and the journal's separate budget for
removed files. The guest checks each result against the prediction and stops at the first
difference. After every request:

- the volume on disk must equal the model's committed state (every file's contents);
- in half the steps, a probe request (in the same instance after a commit, in a fresh one after a
  rollback) must be able to add exactly the model's remaining bytes and entries, and not one more:
  the quota's usage equals the model's, observed through enforcement rather than read out.

Where the request's undo log nears its budget (old bytes of pre-existing files it saved), where the
journal refuses a write depends on how the write's stream is chunked; the model then accepts
`insufficient-space` for that op, reads that file back, and leaves the request's later ops out.

**Found: N-502 (fixed on `ideas/fix-recover-enotdir`, now on `cf/main-next` as 6fdb057).** The
first run shrank to two requests: commit an empty file `d`; then `mkdir d/a` in a request that
fails. `creating` recorded `Created(d/a)`, the mkdir failed with ENOTDIR, and on restart recovery
treated `symlink_metadata`'s ENOTDIR as an error: the object answered 503 `instantiate-failed`
forever. `ideas/quota-model` carries the fix (merged) so its test passes.

**Coverage of one 64-case run** (op and result counts): writes 151 ok / 102 short or refused for
space / 37 refused at the entry limit / 231 other errors; truncates 26 ok / 10 refused for space;
renames 84 ok / 350 refused; unlinks 48 ok; mkdirs 99 ok / 37 at the entry limit; rmdirs 21 ok;
511 committed and 175 rolled-back requests, 330 probes; a few undo-log-full writes. 64 cases take
10-20 s (`QUOTA_MODEL_CASES=1000` for longer runs).

**Does it catch quota bugs?** Mutations of `quota.rs`, each run once with 64 cases:

| Mutation | Result |
| --- | --- |
| a write charges every byte written, not just growth | killed (probe: short of room at 786,432 of 1,048,476 bytes) |
| truncation frees nothing | killed (probe) |
| a directory renamed over an empty one keeps its entry | killed (probe: entry limit one early) |
| unlink keeps the entry | killed (probe) |
| a failed rename's restore forgets the target's bytes | survived: only reachable when the undo log refuses a rename record, which the generator rarely reaches; `rename_log_refusal_restores_target_and_usage` covers it |

Run: `cargo test --locked -p nanovm --test quota_model` (builds the fixture into
`target/test-guest`). Adds `proptest` 1.12 as a dev-dependency (std only: proptest, rand_xorshift,
unarray in `Cargo.lock`; dev builds also rebuild num-traits, chrono, tar and oci-client).
