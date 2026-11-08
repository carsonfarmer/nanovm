# Benchmark history

[history.py](history.py) runs the in-process benchmark (`bench --quick`), appends
one JSON line per run to a history file keyed by commit (with commit date,
rustc, profile, CPU model, core count and the raw bench output), compares the
newest commit with earlier ones, and draws trends.

```sh
python3 tools/bench-history/history.py record --history bench.jsonl      # builds and runs the bench (release)
python3 tools/bench-history/history.py compare --history bench.jsonl     # exit 1: a metric >5% worse
python3 tools/bench-history/history.py chart --history bench.jsonl --out trends.html
python3 tools/bench-history/history.py export --history bench.jsonl > runs.jsonl
```

`compare` takes the median of each metric over a commit's runs, and the median of
the previous five commits run **on the same CPU model and core count** as the
baseline; throughput (`rps`) worse means lower, latency (`p50_ms`, `p99_ms`) and
`cold.kb_per_object` worse means higher. It prints a Markdown table with the
baseline's range, so a "worse" inside the usual spread is visible as such.

`export` writes one row per scenario in the schema of the nice-to-haves branch's
summarizer (`experiments/nice-to-haves/toolkit.py summarize`), which pools runs
by elapsed time rather than averaging percentiles.

[.github/workflows/bench-history.yml](../../.github/workflows/bench-history.yml)
runs this nightly (03:17 UTC) and on demand on the default branch: it builds the
guests, downloads the history from the newest finished run's `bench-history`
artifact, records, compares into the job summary, charts, and uploads the
history and `trends.html` again (kept 90 days; each run carries it forward). A
regression fails the job after the upload, so the next run still has it.

**Noise.** Hosted runners share hardware and change CPU models; `bench --quick`
runs are short. A 5% threshold will flag noise at times: read the baseline
range, rerun the workflow, and confirm on a quiet machine with alternating
builds (the brief's method) before believing a regression. For decisions, a
self-hosted quiet runner is better than the history.

## Evidence

`python3 -m unittest discover -s tools/bench-history -v`: 6 tests on synthetic
bench output (flattening, recording, threshold and baseline, same-machine
filtering, chart, export schema). A real `bench --quick` (dev profile, this
sandbox) was recorded, compared, charted and exported, and the export was
summarized by the nice-to-haves summarizer; see notes/ops.md. The workflow
has not run: schedules and manual runs start only from the default branch.
