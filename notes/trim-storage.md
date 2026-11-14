# Trim: storage (`trim/storage`)

The storage files' runtime (CI's count: non-blank, non-comment lines before
the test module) at 7988cae: ship 604, journal 764, quota 666, committed 423,
lease 271 = **2,728**. Must reach 2,319, aim 2,046.

**Result: 2,548 (-180, -6.6%).** Not "must reach": what is left is behaviour
(below, function by function). Under rustfmt (max_width 120) applied to both
the base and the result, 3,185 -> 2,989 (-196): the base files are not
rustfmt-clean (rustfmt adds 457 lines to them), so neither was reformatted
here; new code follows each file's style and joins no statements.

| File | 7988cae | trim/storage | Delta |
|---|---|---|---|
| ship.rs | 604 | 546 | -58 |
| journal.rs | 764 | 767 | +3 |
| quota.rs | 666 | 637 | -29 |
| committed.rs | 423 | 345 | -78 |
| lease.rs | 271 | 253 | -18 |
| **Total** | **2,728** | **2,548** | **-180** |

journal.rs is +3 because `Journal::changes` now builds the version's manifest
itself (+10), which removed the manifest conversion and a type from
committed.rs (-28); its own cut, sharing `committed::parent_at`, is -7.

## Duplication found, and what happened to it

| What | Where (both places) | Lines | Done |
|---|---|---|---|
| Listing an object's versions | `committed::head` (strict), `ship::published`, `committed::restore`, `committed::settle`, `ship::backlog` (a stat per version again) | 66 | One `committed::list` (versions with sizes, staged names); -58 with the next row |
| Re-checking a chain another layer made or checked | `committed::settle` (identity, sequence, parent of what `prepare` staged under the same lock; the shipper checks again before anything counts); `committed::restore` (re-reads and re-hashes every version `ship::restore` fetched by following parents and checked) | 14 | Removed |
| Record read with its version, conditional write | `lease::Leases::get`/`put` and `ship::Objects::read`/`swap` | 16 | Leases use `ship::Objects` (-16) |
| A restore's install, part by part | `ship::install` (journal, volume, marker through `mark`, versions; a "still staged?" check and cut point each), `ship::sub` (a second `dir_at` with chown) | 25 | The object's directory is staged whole and installed with one `renameat` (-23) |
| Conditional create | `ship::Objects::put` and `Objects::swap` from nothing; `get` in both stores (`read` without the version) | 13 | `put` is `swap` then compare; `get` a default method |
| Random bytes | `ship::written` (`getentropy`) and `journal::key` | 4 | `written` uses `key` |
| `syncfs`, `CLOCK_MONOTONIC` through `unsafe libc` | `ship::sync_fs`, `lease::mono_ns`; rustix has both (`fs`; `time` was already compiled in by another dependency) | 5 | rustix |
| Handing a shipper's per-object state out and back | `ship::run`'s channel and `Option` | 6 | A lock per object |
| A directory by descriptor, a name at a time | `journal::parent_at` and `committed::parent_at`/`dir_at` | 11 | Recovery uses `committed::parent_at` from one descriptor (-7) |
| The commit's changes, twice | `committed::Changes` (from `Journal::changes`) turned into `Manifest` in `prepare`, with a second `stat` per patched file | 20 | `Journal::changes` returns the `Manifest` |
| Walking a tree into a tar | `prepare`'s stack walk and `tar::Builder::append_dir_all` | 7 | `append_dir_all` |
| The stock filesystem host's plain methods | `quota::QuotaView`'s `Host`, `HostDescriptor`, `preopens::Host`: each called the stock view's | 16 | `QuotaFs`'s data is the stock view |
| The app's disk twice in an instance's quota | `quota::Inner::disk`, `Park::disk` | 2 | One |
| One-caller helpers | `quota::QuotaGuard::refuse`, `committed::last`, `lease::Leases::unhold` | 11 | Inlined |
| Two journal calls (two locks, two checks) after a rename | `QuotaGuard::rename`: `renamed`, then `discard_created` | 3 | One call |
| Test-only code counted as runtime | `impl Default for quota::Disk` | 5 | `#[cfg(test)]`, after the tests |

Left for the coordinator (they span other agents' files; the base has them):

| What | Where | Lines |
|---|---|---|
| Recursive chown of a tree only root reaches | `ship::own` and `supervisor::chown_tree` (node's; private, `anyhow`) | 9 here |
| Walking `<app>/<shard>/<object>` | `ship::object_dirs`, `quota::Disk::open`, `app::Objects::load` (node's) | a shared walk saves nothing unless all three use it |
| Free space against the node's reserve | `quota::Disk::low` (`fstatvfs` on a kept descriptor), `app::Objects::disk` (`statvfs` by path; it is handed `disk.reserve` already) | ~6 in app.rs |
| `geteuid` through `unsafe libc` | `ship::owner`, `admin::listen`, `secrets`, `supervisor::app_dirs` | 1 each with rustix `process` |
| A grant's session sent twice | `lease::serve` answers `(grant, grant.session, until)`; `table.rs` reads `(grant, id, until)` | 1-2 |
| Shipping retried in two places | `ship::run` (a second later) and `Supervisor::restore_object` (three tries in a row) | - |

## Behaviour and options removed

- A capture limit (`max_mb`) past 2^44 MiB saturates instead of failing every
  commit with "capture limit overflow"; a sequence past 2^64 isn't checked.
- Names in `committed/` that aren't a version or a staged version are passed
  over everywhere (the shipper already did); commits and restores used to
  fail on them ("invalid committed version name/entry"). Only the worker
  writes there, and only `prepare`/`publish`/`settle` name versions.
- `settle` no longer refuses a decided staged version that doesn't follow
  ("out-of-order committed version"); `committed::restore` no longer refuses
  a broken chain ("broken chain of committed versions"); `prepare` no longer
  refuses a volume entry that isn't a file or a directory ("unsupported
  capture file type"). See the commits for why each is guaranteed elsewhere.
- `object_at` no longer makes an object's directories for a marker: marking
  an object whose directory went is an error, not a root-made directory.
- A renewal whose create of the node record finds it there reads it back and
  carries on if it names this session (its own create, answered late).
- `experiments/fs-backends/` and `examples/committed_state.rs` (with its two
  tests) are deleted, as on `review/main` (the coordinator's request).
- No configuration key was removed.

## What remains, and why

Each line below is behaviour a test or an owner decision needs; none
repeats another.

- **ship.rs (546).** `Store` (10) and `store` (9): the two kinds of store.
  `Dir` (~90 with `written`, `sync_names`, `tag`): a directory store must
  sync what it writes (`object_store`'s local files don't, and can't swap a
  record), make a put never replace (`link`), and swap under a lock file.
  `Objects` (~35): S3's conditional writes. `Head`, keys, `head` (18).
  Marker (`marker`, `mark`, `shipped`, 11), `stored`/`caught_up` (14,
  `remote_ack`, N-725). `ship_object` (55): fencing by lease epoch, the
  identity and parent checks (B-504), the known head (N-728), catching up a
  lost marker, pruning by descriptor (B-502). `run` (35) with `Shipping`:
  per-object failures and retries, the changed-directory skip, `SHIPPING` at
  once, `Held`. `backlog` (19, B-501's visibility). `claim`/`current`/
  `discard` (46, W-10: never serve older state than the head, a durable drop
  before the claim, a discard cut short). `restore`/`install`/`recover`/
  `staging`/`owner`/`own` (~120: B-50x, crash-safe at each cut point, root
  never through a symlink). `pause`/`resume`/`paused` (17).
- **journal.rs (767).** The undo journal (round 1-2, hardened by N-5 to
  N-690): records, signing, recovery that resumes, path-length refusal,
  concurrent syncs, the object lifecycle (`place`, `Drop`). Capture adds
  `changes` and the two commit gates. `fd_path`'s macOS branch (10) is the
  one piece no supported build uses (the crate already needs Linux's
  `syncfs`); PLAN keeps macOS as a goal, so it is the owner's call.
- **quota.rs (637).** The WASI filesystem wrapper (a method per WASI call
  that changes bytes or entries), the object quota, the app budget and the
  reserve (B-302), the `/tmp` accounting, rename staging and its rollback.
- **committed.rs (345).** `prepare` (77: snapshot or change, bounded bytes
  while writing, B-501's snapshot skip), `publish`, `settle` (the two
  decisions on recovery), `apply` (71, S4: every name by descriptor, files
  and directories only, patched files that are one file), `restore`,
  `list`, `read_version`, `dir_at`/`parent_at`/`listing`/`remove_at`.
- **lease.rs (253).** The lease protocol (`renew`, `acquire`, `alive`,
  `release`, sessions, epochs), the worker's side, `serve` with N-726's
  refusal of objects whose history is broken.

Further cuts would remove behaviour, each the owner's call: the macOS
`fd_path` branch (10); the directory store's put through its swap (about 15,
at the cost of hashing each shipped version again); the known head (6, a
round trip per version); the shipper's changed-directory skip (4, CPU per
20 ms round); N-726's refusal (14); stored times in `versions` (4).

## Tests

Full suite (`env -u RUST_BACKTRACE RUST_TEST_THREADS=1 cargo test --locked -p
nanovm --no-fail-fast`), at the last runtime change: lib 194 passed (1
ignored), configs 1, dev 11, examples 29, integration 128 (14 ignored), js 17:
all passed, the same counts as beta/integration's final row
(notes/beta-integration.md) but for `committed_state`'s 2, deleted. SQLite
(`--test integration sqlite_ -- --ignored`): 12 passed. `cargo check --locked
-p nanovm --bin nanovm`: clean. Unit tests for the storage files after every
commit: 89 passed, 1 ignored. Changed test: `a_restore_cut_short_is_finished_or_undone` counts 5
cut points, not 9 (install is one swap); mutation-checked (recover installing
undecided restores, install keeping the old directory: both fail it).
