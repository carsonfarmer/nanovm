# Ops notes

Operations and packaging work from IDEAS.md (sections E, F, L, M), one branch per
item, each from `cf/main-next`. Plain language; numbers and how to reproduce.

## E-63 filesystem matrix (`ideas/fs-matrix`)

**What:** `experiments/fs-matrix/` (`run.sh`, `fsync.py`, `summarize.py`, raw
results and a README with the table). Measurement only: `data_dir` on this
disk's ext4, tmpfs, and ext4/ext4+discard/XFS/btrfs on loop devices; fsync
latency for the journal's two patterns and a dev node's `POST /incr` serially
and at 64 concurrent; 3 alternating rounds.

**Results (six alternating rounds over two sessions, load average 19–34, so
relative only):** one `/incr` makes 9 flushes (6 `fsync` + 3 `fdatasync`,
`strace -c` over 200 requests, the same on `0ac2849` and `3dafe33`); tmpfs (not
durable) is 9–33× faster serially and 11–80× concurrently by median; btrfs held at
39–46/s at 64 concurrent in all six rounds (p99 4–5 s) while XFS never fell below
85/s; ext4's online `discard` lowered the loop setup's concurrent rate in five of
six rounds (median 60 vs 100/s; prefer `fstrim`); XFS led ext4 on the same setup
in five of six concurrent rounds and had the fastest write-rename-fsync in all
six. Hand-off for the storage owners: flush count per request, not the
filesystem, is the lever. `run.sh` now records non-200 statuses (they were 500s).

**Not measured:** real devices for XFS/btrfs, NVMe/cloud volumes, release
builds, a quiet machine. Rerun with `run.sh` on the target hardware.
