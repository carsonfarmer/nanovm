# beta/deployable: the ops work on round 3

The production audit found nothing deployable on one branch: round 3
(`cf/round3`) has the SDK, `nanovm run`, `publish` and the examples, and
`ideas/ops` had the deploy assets, runbooks and release process on the older
`3dafe33` base, with reference configs that no longer parse. `beta/deployable`
merges `ideas/ops` onto round 3 (with the no-compat cleanup, 9784334), fixes
what round 3 broke in it, tests every shipped config, and checks the
single-node deployment end to end on this host.

## What was merged

`origin/ideas/ops` (dd3299c): 19 commits from ten branches, 52 files, about
3,400 lines, all docs, units, scripts, configs and tests:

| Item | What |
|---|---|
| L-111 | `deploy/systemd`: hardened units, sysctl and cgroup drop-ins, reference configs, nspawn test |
| L-113/L-114 | `deploy/single-node`: guide, Caddyfile, `install.sh`, `check-host.py`, sizing, e2e test |
| L-110 | `deploy/container`: Dockerfile, run flags, reference configs, local test |
| L-116 | `tools/release` and `.github/workflows/release.yml` |
| E-59 | `tools/bench-history` and `.github/workflows/bench-history.yml` |
| F-71 | `docs/ops/errors.md` and `errors-check.sh` |
| L-115 | `docs/ops/runbooks.md` and four drills |
| E-63 | `experiments/fs-matrix` |
| F-67 | `deploy/monitoring`: Prometheus rules with promtool tests |
| N-550/551/552 | README's map count; the worker's parent check; workers from `/proc/self/exe` |

## Conflicts, and how they were resolved

Round 3's code wins every conflict; ops brings no runtime code.

- `main.rs`, `supervisor.rs`: round 3 already had N-551 (ea4019b) and N-552
  (37dd88e), reworked for spare workers (091c810) and leases. Kept round 3's;
  ops' older spawn and three-part worker spec are dropped. `jail.rs` and
  `tests/integration.rs` merged clean to round 3's.
- `ISSUES.md`: the auto-merge added N-551 and N-552 a second time; removed.
  N-550 is new on round 3.
- `README.md`: round 3's address-space line kept, ops' corrected map count
  (24 per slot plus 4,096) taken.
- `notes/ops.md`: ops' full notes taken (round 3 had only its N-552 section,
  the same text), with a pointer here.

One round-3 regression showed up only through the merge: the spare-worker port
(091c810) dropped N-552's `arg0("nanovm")`, so workers showed as
`/proc/self/exe worker` and ops' own checks (`pgrep -f "nanovm worker"`) found
none (B-601, fixed).

## What changed on top

| Commit | What | Runtime lines |
|---|---|---|
| 2e3ece0 | the merge | 0 |
| 4ea16e6 | `tests/configs.rs`; `ProxyConfig::load`; configs use `proxy` and `nodes` (B-602) | +5 |
| 9f75465 | workers show as `nanovm worker` again (B-601) | 0 |
| 2480513 | check-host: address-space preflight, round 3's capture (B-603) | 0 |
| 2588bae | units test: the spare worker, `apps/worker-<n>` | 0 |
| 7a79a20 | errors guide, runbooks, disk-full drill as round 3 behaves; rule 1 in upgrade/rollback | 0 |
| 8b17f8a | CI runs the ops tooling; the release workflow ships disabled | 0 |
| 946f3e8 | the store drop-in (B-604) and a store mode in the units test | 0 |
| 2f639fa | the single-node beta operator guide | 0 |

Runtime: 7,410 → 7,415 lines (CI's count).

## The config test

`crates/nanovm/tests/configs.rs` walks the repository (not `target/`, `.git`)
and loads each node `*.toml` with `NodeConfig::load`, each `proxy.toml` with
`ProxyConfig::load` (moved out of `main.rs` so `nanovm proxy --config` and the
test share it), and each `nanovm.json` as an `AppSpec`: 5 TOML configs and 25
manifests (23 examples, 2 guests). It reports every failure at once and checks
it found the deploy, demo and guest files. (4ea16e6's message says 22
manifests; it is 25.)

- Before the fix, on the merged configs: fails on four files, `unknown field
  publish_to` (deploy/container/nanovm.toml:9, deploy/systemd/nanovm.toml:15)
  and `unknown field node` (deploy/container/proxy.toml:5,
  deploy/systemd/proxy.toml:7). The round-3 binary refuses the same files
  (`nanovm serve`/`nanovm proxy --config`, same errors).
- After: passes.
- Mutations, each restored (`git diff` empty after): serde-renaming
  `NodeConfig.proxy` and `ProxyConfig.nodes` fails it on all four deploy
  configs; serde-renaming `AppSpec.limits` fails it on every manifest with
  limits.

No aliases for the old keys (AGENTS.md rule 1).

## Verification on this host

**The host:** Ubuntu 24.04, Linux 6.18, 4 cores, cgroup v1 (hybrid), root.
systemd 255 installed but not PID 1, so units run in `systemd-nspawn` booting
the host's own OS (as ops did). Docker 29.9.2 present; its daemon wasn't
running and was started for the container test (overlayfs, cgroup v1, default
seccomp), Docker Hub pulls work. No Podman or buildah. Caddy 2.6.2 (Ubuntu
package), promtool 2.45.3, shellcheck 0.9.0, restic 0.16.4. Not present:
`oha`, `otelcol-contrib`, `actionlint`, WASI SDK. Disk: 11 GB free at the start,
4.6 GB at the end, shared by six agents.

All with a dev build of this branch (`cargo build -p nanovm --features dev
--bin nanovm --example dev-registry`), guests from `scripts/build-guests.sh`.

| Check | Result |
|---|---|
| Single-node end to end: `SCRIPT=deploy/single-node/test/e2e.sh MODES=plain deploy/systemd/test/nspawn.sh` | 8/8: `install.sh` and its host check; Caddy (internal CA, verified TLS) → proxy (DoH stand-in) → node → jailed worker; count 3; a client's `nanovm-app` ignored; no record 404; 8080/8444 on 127.0.0.1 only; spans from proxy (7), supervisor (5), worker (5); count survives a restart |
| Units: `deploy/systemd/test/nspawn.sh`, four modes | plain 16, cgroup 19, capture 17, store 18 checks: all pass (70). Before 2588bae each of the first three failed "two workers running" (three: two apps and the spare). Store mode failed 2 checks until the drop-in had `CAP_DAC_OVERRIDE` (B-604) |
| `systemd-analyze verify` | no messages on both units with the cgroup and CPU drop-ins (ExecStart pointed at the build; a bad `AllowedCPUs` in a drop-in is reported, so drop-ins are read) |
| `caddy validate` | Valid configuration |
| `check-host.py` on `deploy/systemd/nanovm.toml` | ok; warns: data_dir not created yet, ext4 mounted with `discard`, `RUST_BACKTRACE` set in this shell. 7 unit tests pass; the new address-space test fails with `GUARD = 0` or the check disabled |
| `docs/ops/errors-check.sh` | 20/20 checks give the documented status and reason (18 reasons) |
| `drills/disk-full.sh` | round 3 differs from ops' text, which was rewritten: counter's own 507 x2, overwrite in place commits while its KV write fails, a new object's first write `500 commit-failed` "journal requires recovery", reads 200; freed: 3 → 4, new object 1. The first run left its tmpfs mounted (workers outlive the supervisor briefly); the drill now retries the unmount |
| `drills/backup-restore.sh` | count 3 and KV "kept" before and after restic backup and restore into a fresh root; uids 200000/200001 kept; next write 4 |
| Container: `deploy/container/test/local.sh` (stand-in base, local binary) | 11/11; image 92 MB; stop 183 ms with `--init`, 3,192 ms as PID 1; workers start as PID 1 |
| Container on the real `debian:trixie-slim` with the image's own configs | node ready 204, proxy answers 502 (no network for its resolver); 200 MB with the debug binary |
| promtool `check config`, `check rules`, `test rules` | pass (14 rules, 5 test groups) |
| `package.sh` | identical archives twice, checksum OK, `deploy/` included, a non-nanovm binary and tag `latest` refused |
| Python tests (check-host, bench history, changelog) | 7 + 6 + 4 pass |
| shellcheck `-S warning` on every ops script | clean |
| `bench-history`: the JSON keys `history.py` reads | all present in `bench.rs`/`support/load.rs` (static check; the bench was not run) |

**Rust tests:** see the end of this file.

## Not run, and why

- **A full image build**: the build stage's release build needs several GB of
  disk this shared host didn't have; the build context resolves offline and its
  base tag exists.
- **`nanovm publish` to a registry, signed apps**: no registry credentials or
  signing identity here; apps came from the dev registry in dev mode. The
  end-to-end test is dev mode for that reason.
- **Real ACME, real DNS, an S3 store, `otelcol-contrib`, `oha`** (so not the
  Collector-outage drill, nor `bench --quick`), `AllowedCPUs` enforcement and
  cgroup v2 controllers (cgroup v1 here), arm64, Podman.
- **The binary-swap drill**: it compares two builds with different worker specs;
  N-552 is in round 3 and its behaviour is covered by the units test's restart.
- **The release and bench-history workflows** (need GitHub).
- The fs-matrix numbers are from before round 3 and weren't measured again.

## Owner questions

1. **Upgrades lose data.** Under rule 1 an upgrade starts the node on a fresh
   data directory (runbooks, rewritten), so beta developers' objects don't
   survive a nanovm upgrade. Recommended: say so to the beta developers, upgrade
   rarely, and decide before wider use whether a release may promise to keep
   v1's on-disk formats.
2. **Release workflow.** `release.yml` publishes a GitHub release with
   attestations on a pushed `v*` tag; it ships as `release.yml.disabled`.
   Recommended: enable it (rename) once you want tagged releases.
3. **Nightly bench-history workflow.** It publishes nothing (a 90-day workflow
   artifact, `contents: read`) and was left enabled. Recommended: keep it.
4. **`CAP_DAC_OVERRIDE` with a capture store** (B-604). Recommended: keep it in
   the store drop-in only; separately, fix `ship::run` so a filesystem EACCES is
   logged rather than taken for "no lease".
