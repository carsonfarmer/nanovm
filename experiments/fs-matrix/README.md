# Filesystem matrix for durable writes (E-63)

Measurement only, for operator docs: which filesystem to put `data_dir` on.
Every `POST /incr` on the counter guest is a durable write: the undo journal
records, then the commit syncs, before the response is released.

**What one `/incr` costs:** `strace -c` on the worker over 200 serial requests on
ext4 counted, per request, **6 `fsync` + 3 `fdatasync`**, 8 `openat`, 2
`unlink`, 1 `rename`, 1 `pwrite64`, 1 `ftruncate`. So durable-write latency is
about nine device flushes; filesystem flush cost dominates everything else.

## Results (2026-11-08 and -09)

`run.sh`, six alternating rounds in two sessions (order reversed on even
rounds): rounds 1–3 with a dev build of `cf/main-next` `0ac2849`, rounds 4–6 of
`3dafe33` (W-9 local capture, off; the same 9 flushes per `/incr`). nanovm
`opt-level` 1, dependencies 3; 4-core Xeon VM (virtio disk, Linux 6.18),
xfsprogs 6.6.0, btrfs-progs 6.6.3, e2fsprogs 1.47.0. `fsync.py`: 500 iterations.
`/incr`: 300 serial requests to one object; 2,000 at 64 concurrent over 100
objects (oha 1.16.0). Median (min–max) of the six rounds.

| filesystem | fdatasync 4 KiB p50 ms | write+fsync+rename+dir fsync p50 ms | `/incr` serial /s | serial p50 ms | `/incr` c64 /s | c64 p99 ms |
|---|---:|---:|---:|---:|---:|---:|
| ext4 (this disk, `discard`) | 2.56 (0.60–4.23) | 5.12 (3.69–12.3) | 42 (21–54) | 19.9 (12.3–28.8) | 320 (153–432) | 748 (578–1544) |
| ext4 on loop | 0.61 (0.38–3.99) | 4.98 (1.98–8.43) | 43 (26–82) | 16.2 (9.7–28.0) | 100 (36–122) | 2477 (1543–12465) |
| ext4 on loop, `discard` | 1.58 (0.68–3.49) | 6.07 (3.20–10.6) | 31 (25–66) | 23.4 (12.6–32.7) | 60 (39–98) | 3656 (2287–5066) |
| XFS on loop | 0.73 (0.44–3.44) | 2.05 (1.51–2.80) | 108 (20–155) | 7.3 (3.1–13.3) | 139 (85–208) | 1438 (965–3220) |
| btrfs on loop (`discard=async`) | 2.46 (1.61–6.85) | 5.06 (3.28–9.30) | 40 (16–54) | 22.5 (16.8–30.3) | **44 (39–46)** | **4664 (4076–5160)** |
| tmpfs (not durable) | 0.001 | 0.006 | 1016 (426–2577) | 0.85 (0.36–0.99) | 3500 (1832–4588) | 56 (36–128) |

**Load average 19–34 throughout**: other agents were building on the same 4
cores and disk. Absolute numbers are inflated and ranges are wide. 312 of
124,800 responses were not 200 (by status 500; the counter's CPU budget is
50 ms of wall time in polls, which an overloaded host inflates), almost all in
runs whose p99 was seconds.

What held in every round, or nearly:

- **Flushes are the cost.** On tmpfs, where `fsync` is a no-op, `/incr` ran
  faster than every disk row in all six rounds: by median 9–33× serially and
  11–80× at 64 concurrent. It is not durable: never put `data_dir` there except
  for tests.
- **btrfs stayed slow under concurrency:** 39–46/s at 64 concurrent in all six
  rounds, p99 4–5 s. ext4 on loop fell as low in two rounds of the second,
  busier session but reached 86–122/s in the other four; XFS never went below
  85/s. Serially btrfs matched ext4. Don't use it for
  `data_dir` without measuring your own workload.
- **Online `discard` cost ext4-on-loop concurrent throughput in five of six
  rounds** (median 60/s against 100/s; 29–46% lower in the three first-session
  rounds). Each commit unlinks the replaced file and its journal, and `discard`
  trims the freed blocks as the filesystem commits. Prefer periodic `fstrim`
  (Ubuntu's `fstrim.timer`) to the `discard` mount option.
- **XFS was ahead of ext4 on the same loop setup** in five of six rounds at 64
  concurrent (median 139 against 100/s) and four of six serially; its
  write-rename-fsync pattern was the fastest disk row in every round. The
  ranges overlap: call it at least as good.

Not comparable: the native ext4 row against the loop rows (different device
paths; the loop files live on that same ext4, so loop rows carry its cost too).
Not measured: XFS/btrfs on a real device, NVMe or cloud volumes, release builds,
a quiet machine.

## Reproduce

```sh
cargo build -p nanovm --features dev --bin nanovm --example dev-registry
sudo apt-get install -y xfsprogs btrfs-progs      # optional rows
sudo OHA=/path/to/oha experiments/fs-matrix/run.sh /tmp/fs-out 3
```

Raw rows (with each round's build): [results.jsonl](results.jsonl);
`python3 -I experiments/fs-matrix/summarize.py <results.jsonl>` makes the table.
The script needs root (loop devices, tmpfs), makes its own 2 GiB images under
`/var/tmp` (workers run as other uids, so `data_dir` can't sit under a 0700
directory), and removes everything when it exits.
