# Ops notes

Operations and packaging work from IDEAS.md (sections E, F, L, M), one branch per
item, each from `cf/main-next`. Plain language; numbers and how to reproduce.

## E-59 benchmark history (`ideas/bench-history`)

**What:** `tools/bench-history/history.py` (`record` runs `bench --quick` and
appends one JSON line keyed by commit with machine, toolchain and raw output;
`compare` flags metrics >5% worse than the median of the previous five commits
on the same CPU model and core count; `chart` writes an HTML page of SVG trend
lines; `export` emits rows for the nice-to-haves summarizer) and
`.github/workflows/bench-history.yml` (nightly 03:17 UTC and manual, on the
default branch; carries the history forward through its own artifact).

**Evidence:** 6 unit tests pass. A real `bench --quick` here (dev profile; the
release profile would rebuild every dependency) was recorded (55 metrics),
charted (28 trend lines), exported and summarized by
`origin/nice-to-haves:experiments/nice-to-haves/toolkit.py summarize` (9 groups,
no schema errors), and a copy with warm GET 10% slower was flagged (exit 1).
That run, at load average ~17, also shows why one run is not evidence: warm
`/incr` serial was 54/s and 3 warm GETs were killed by the 50 ms CPU budget.
The workflow has not run: scheduled and manual runs start only from the default
branch, so pushing this branch runs only CI.
