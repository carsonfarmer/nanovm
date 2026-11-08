# Filesystem matrix for durable writes (E-63)

Measurement only, for operator docs: which filesystem to put `data_dir` on.
Every `POST /incr` on the counter guest is a durable write: the undo journal
records, then the commit syncs, before the response is released.

**What one `/incr` costs:** `strace -c` on the worker over 200 serial requests on
ext4 counted, per request, **6 `fsync` + 3 `fdatasync`**, 8 `openat`, 2
`unlink`, 1 `rename`, 1 `pwrite64`, 1 `ftruncate`. So durable-write latency is
about nine device flushes; filesystem flush cost dominates everything else.

## Results (2026-11-08)

`run.sh` with 3 alternating rounds (order reversed on even rounds), a dev build
of `cf/main-next` (nanovm `opt-level` 1, dependencies 3), 4-core Xeon VM
(virtio disk, Linux 6.18), xfsprogs 6.6.0, btrfs-progs 6.6.3, e2fsprogs 1.47.0.
`fsync.py`: 500 iterations. `/incr`: 300 serial requests to one object; 2,000
requests at 64 concurrent over 100 objects (oha 1.16.0). Median (min–max).

| filesystem | fdatasync 4 KiB p50 ms | write+fsync+rename+dir fsync p50 ms | `/incr` serial /s | serial p50 ms | `/incr` c64 /s | c64 p99 ms |
|---|---:|---:|---:|---:|---:|---:|
| ext4 (this disk, `discard`) | 2.17 (0.98–3.26) | 3.96 (3.69–12.3) | 45 (21–48) | 20.2 (17.3–28.8) | 288 (153–432) | 788 (578–1544) |
| ext4 on loop | 0.66 (0.52–1.08) | 2.91 (2.52–7.05) | 53 (26–82) | 12.8 (9.7–28.0) | 119 (86–122) | 1848 (1543–2710) |
| ext4 on loop, `discard` | 1.09 (0.68–1.97) | 4.01 (3.70–10.6) | 53 (28–66) | 14.7 (12.6–31.6) | 64 (55–87) | 3292 (2287–4021) |
| XFS on loop | 0.57 (0.47–0.89) | 2.10 (1.99–2.60) | 97 (55–118) | 8.3 (7.8–13.3) | 157 (120–180) | 1063 (965–1813) |
| btrfs on loop (`discard=async`) | 2.46 (1.92–2.47) | 4.52 (3.96–5.59) | 48 (28–49) | 19.9 (18.8–25.0) | **45 (43–46)** | **4281 (4076–5046)** |
| tmpfs (not durable) | 0.001 | 0.006 | 880 (544–1533) | 0.94 (0.61–0.99) | 2238 (1832–4051) | 127 (40–128) |

Non-200 responses: 6 of 41,400. **Load average 25–34 throughout**: other agents
were building on the same 4 cores and disk. Absolute numbers are inflated and
ranges are wide; only differences that hold in every round are worth repeating.

What holds in every round:

- **Flushes are the cost.** On tmpfs, where `fsync` is a no-op, `/incr` ran
  faster than every disk row in every round: by median, 9–20× serially and
  8–50× at 64 concurrent. It is not durable: never put `data_dir` there except
  for tests.
- **btrfs is the worst under concurrency:** 43–46/s at 64 concurrent, the
  lowest in every round, against 55–432/s for the other disk rows, with p99
  above 4 s. Serially it matches ext4. Don't use it for `data_dir` without
  measuring your own workload.
- **Online `discard` cost ext4-on-loop 29–46% of its concurrent rate** (87, 64,
  55/s against 122, 119, 86/s, lower in every round). Each commit unlinks the
  replaced file and its journal, and `discard` trims the freed blocks as the
  filesystem commits. Prefer periodic `fstrim` (Ubuntu's `fstrim.timer`) to the
  `discard` mount option.

Weaker: XFS beat ext4 on the same loop setup in two of three rounds, serially
and concurrently (the third was a loss and a tie); the ranges overlap, so call it
no worse, not better.

Not comparable: the native ext4 row against the loop rows (different device
paths; the loop files live on that same ext4, so loop rows carry its cost too).
Not measured: XFS/btrfs on a real device, NVMe or cloud volumes, `nobarrier`-style
options, release builds, a quiet machine.

## Reproduce

```sh
cargo build -p nanovm --features dev --bin nanovm --example dev-registry
sudo apt-get install -y xfsprogs btrfs-progs      # optional rows
sudo OHA=/path/to/oha experiments/fs-matrix/run.sh /tmp/fs-out 3
```

Raw rows: [results-2026-11-08.jsonl](results-2026-11-08.jsonl);
`python3 -I experiments/fs-matrix/summarize.py <results.jsonl>` makes the table.
The script needs root (loop devices, tmpfs), makes its own 2 GiB images under
`/var/tmp` (workers run as other uids, so `data_dir` can't sit under a 0700
directory), and removes everything when it exits.
