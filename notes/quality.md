# Quality notes (quality agent, round 3)

Test and verification work from IDEAS.md sections D and G. Each item is on its own branch from
`cf/main-next`; this file collects the notes. Commands run from the repository root.

## D-51: soak test (`ideas/soak`)

**What it is.** `crates/nanovm/tests/soak.rs` (ignored by default) and `scripts/soak.sh [seconds]
[csv]`. Two in-process nodes: `counter` with 64 live slots over 600 objects (constant LRU
eviction and cold starts) and `hostile` with 32 slots over 200 objects. Six clients run a fixed,
seeded mix: counter increments (a synced `/data` write each), reads and key-value put/get/delete;
hostile commits and rollbacks (files and KV), 1 MiB `/tmp` writes, echoes up to 64 KiB, and every
kill the host makes (CPU, memory, trap, resource table: 8 % of requests). Every 30 s it samples
the process's RSS (`VmRSS`), open fds (`/proc/self/fd`) and memory mappings (`/proc/self/maps`)
into a CSV. It compares the median of the second quarter (warm) with the last quarter: RSS may
grow 10 % or 32 MiB, fds 32, mappings 100. Unexpected answers are counted, not fatal (at most
0.1 %; `busy`, `cpu` and `timeout` are tolerated on a loaded host).

**Result: 30 minutes at `cf/main-next` 98f25be** (debug test build, 4 shared cores under other
agents' builds; raw samples in `notes/quality-soak.csv`): 320,466 requests (178/s), no unexpected
answers, passed.

| | first sample (30 s) | second quarter, median (min-max) | last quarter, median (min-max) |
| --- | --- | --- | --- |
| RSS | 93 MiB | 101 MiB (97-130) | 99 MiB (96-175) |
| open fds | 209 | 209 (206-216) | 210 (206-216) |
| mappings | 2,567 | 2,943 (2,927-2,991) | 3,015 (3,015-3,047) |

RSS and fds are flat (RSS spikes are the `/alloc` and `/echo` peaks being sampled). Mappings rise
fast while pool slots and caches warm (2,567 → 2,927 in 6.5 minutes) and then creep, slowing:
+16 per 3 minutes around 10-15 minutes, +8 to +16 per 3 minutes after. It passed the threshold,
but whether the creep levels off is not shown by 30 minutes: the 24-hour run IDEAS asks for, with
`/proc/self/maps` diffs to tell anonymous (allocator) from file mappings, is the next step.

Run: `scripts/soak.sh 1800 soak.csv` (guests built with `scripts/build-guests.sh`).
