#!/usr/bin/env python3
"""Tables from drive.py's raw.jsonl: per measure, builds side by side, the
median over rounds and its range (min-max).

    python3 summarize.py notes/bench-numbers/raw.jsonl > notes/bench-numbers/tables.md

Also writes summary.csv next to the input (one row per build, phase, case,
concurrency and metric: median, min, max, rounds).
"""

import csv
import json
import statistics
import sys
from collections import defaultdict
from pathlib import Path


def fmt(v):
    if v is None:
        return "n/a"
    if abs(v) >= 1000:
        return f"{v:,.0f}"
    if abs(v) >= 10:
        return f"{v:.0f}"
    return f"{v:.2f}"


def cell(vals):
    vals = [v for v in vals if v is not None]
    if not vals:
        return "n/a"
    m = statistics.median(vals)
    return fmt(m) if len(vals) == 1 else f"{fmt(m)} ({fmt(min(vals))}–{fmt(max(vals))})"


def main():
    src = Path(sys.argv[1])
    rows = [json.loads(line) for line in src.read_text().splitlines() if line.strip()]
    builds = []
    for r in rows:
        if "build" in r and r["build"] not in builds:
            builds.append(r["build"])
    groups = defaultdict(lambda: defaultdict(list))
    for r in rows:
        if r.get("phase") in (None, "machine", "start", "inproc", "inproc_writes"):
            continue
        key = (r["phase"], r.get("case", ""), r.get("conc", ""))
        groups[key][r["build"]].append(r)

    metrics = {
        "node_cold": ["p50_ms", "p99_ms", "max_ms"],
        "app_cold": ["p50_ms", "p99_ms", "max_ms"],
        "density": ["kb_per_object", "objects_per_gib"],
        "syncs": [],
    }
    default = ["rps", "p50_ms", "p99_ms", "p999_ms", "node_us", "proxy_us"]
    out = csv.writer(open(src.with_name("summary.csv"), "w", newline=""))
    out.writerow(["phase", "case", "conc", "metric", "build", "median", "min", "max", "rounds"])
    phase_seen = None
    for key in sorted(groups, key=lambda k: (k[0], k[1], str(k[2]).zfill(4))):
        phase, case, conc = key
        ms = metrics.get(phase, default)
        if phase == "syncs":
            names = sorted({n for b in groups[key].values() for r in b for n in r.get("per_request", {})})
            ms = names
        if phase != phase_seen:
            print(f"\n### {phase}\n")
            print("| case | conc | metric | " + " | ".join(builds) + " |")
            print("|---|---:|---|" + "---:|" * len(builds))
            phase_seen = phase
        for m in ms:
            cells = []
            for b in builds:
                if phase == "syncs":
                    vals = [r.get("per_request", {}).get(m) for r in groups[key].get(b, [])]
                else:
                    vals = [r.get(m) for r in groups[key].get(b, [])]
                vals = [v for v in vals if v is not None]
                cells.append(cell(vals))
                if vals:
                    out.writerow([phase, case, conc, m, b, statistics.median(vals), min(vals), max(vals), len(vals)])
            print(f"| {case} | {conc} | {m} | " + " | ".join(cells) + " |")


if __name__ == "__main__":
    main()
