# Ops notes

Operations and packaging work from IDEAS.md (sections E, F, L, M), one branch per
item, each from `cf/main-next`. Plain language; numbers and how to reproduce.

## E-63 filesystem matrix (`ideas/fs-matrix`)

**What:** `experiments/fs-matrix/` (`run.sh`, `fsync.py`, `summarize.py`, raw
results and a README with the table). Measurement only: `data_dir` on this
disk's ext4, tmpfs, and ext4/ext4+discard/XFS/btrfs on loop devices; fsync
latency for the journal's two patterns and a dev node's `POST /incr` serially
and at 64 concurrent; 3 alternating rounds.

**Results (load average 25–34, so relative only):** one `/incr` makes 9 flushes
(6 `fsync` + 3 `fdatasync`, `strace -c` over 200 requests); tmpfs (not durable)
is 9–20× faster serially and 8–50× concurrently by median; btrfs was lowest at
64 concurrent in every round (43–46/s vs 55–432/s, p99 > 4 s); ext4's online
`discard` cut the loop setup's concurrent rate by 29–46% in every round (prefer
`fstrim`); XFS no worse than ext4 on the same setup (ahead in two of three
rounds). Hand-off for the storage owners: flush count per request, not the
filesystem, is the lever.

**Not measured:** real devices for XFS/btrfs, NVMe/cloud volumes, release
builds, a quiet machine. Rerun with `run.sh` on the target hardware.
