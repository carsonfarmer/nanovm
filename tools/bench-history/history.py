#!/usr/bin/env python3
"""Benchmark history: run `bench --quick`, keep one JSON line per run keyed by
commit, flag regressions and chart trends.

    tools/bench-history/history.py record --history bench.jsonl          # runs the bench
    tools/bench-history/history.py record --history bench.jsonl --bench-json out.json
    tools/bench-history/history.py compare --history bench.jsonl [--threshold 0.05]
    tools/bench-history/history.py chart --history bench.jsonl --out trends.html
    tools/bench-history/history.py export --history bench.jsonl > runs.jsonl

`compare` checks the newest commit against the median of the previous
`--baseline` commits (5) run on the same CPU model and core count, and exits 1
if any metric is worse by more than the threshold. `export` writes rows in the schema of the nice-to-haves branch's
`toolkit.py summarize` (experiments/nice-to-haves), which pools runs by elapsed
time. Wall-clock numbers from a shared runner are noisy: see README.md.
"""

import argparse
import datetime
import html
import json
import os
import platform
import statistics
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
# Scenario paths in bench.rs's JSON whose value is a load run.
RUNS = {
    "cold": ("cold", "run"),
    "warm_get": ("warm_get",),
    "warm_serial": ("warm_serial",),
    "warm_incr": ("warm_incr",),
    "warm_incr_serial": ("warm_incr_serial",),
    "recover": ("recover",),
    "churn": ("churn",),
    "neighbours": ("neighbours", "baseline"),
    "neighbours_attacked": ("neighbours", "under_attack"),
}
# Metric suffix -> True when higher is better.
DIRECTION = {"rps": True, "p50_ms": False, "p99_ms": False, "kb_per_object": False}


def git(*args):
    return subprocess.run(["git", "-C", str(REPO), *args], check=True, capture_output=True, text=True).stdout.strip()


def machine():
    cpu = "unknown"
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                cpu = line.split(":", 1)[1].strip()
                break
    except OSError:
        pass
    return {"cpu": cpu, "cores": os.cpu_count(), "kernel": platform.release(),
            "runner": os.environ.get("RUNNER_NAME", platform.node())}


def metrics(bench):
    """Flat {scenario.metric: value} from bench.rs's JSON."""
    out = {}
    for name, path in RUNS.items():
        run = bench
        for key in path:
            run = run.get(key, {}) if isinstance(run, dict) else {}
        for m in ("rps", "p50_ms", "p99_ms", "failed", "requests", "elapsed_ms"):
            if isinstance(run.get(m), (int, float)):
                out[f"{name}.{m}"] = run[m]
    if isinstance(bench.get("cold", {}).get("kb_per_object"), (int, float)):
        out["cold.kb_per_object"] = bench["cold"]["kb_per_object"]
    return out


def run_bench(profile, out):
    cmd = ["cargo", "run", "--locked", "-p", "nanovm", "--example", "bench"]
    if profile == "release":
        cmd.insert(2, "--release")
    env = {k: v for k, v in os.environ.items() if k != "RUST_BACKTRACE"}
    subprocess.run([*cmd, "--", "--quick", "--json", str(out)], check=True, cwd=REPO, env=env)


def record(args):
    if args.bench_json:
        bench = json.loads(Path(args.bench_json).read_text())
    else:
        with tempfile.TemporaryDirectory() as d:
            out = Path(d) / "bench.json"
            run_bench(args.profile, out)
            bench = json.loads(out.read_text())
    entry = {
        "commit": git("rev-parse", "HEAD"),
        "commit_date": git("log", "-1", "--format=%cI"),
        "dirty": bool(git("status", "--porcelain", "--untracked-files=no")),
        "recorded": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
        "mode": "quick",
        "profile": args.profile,
        "rustc": subprocess.run(["rustc", "-V"], capture_output=True, text=True).stdout.strip(),
        "machine": machine(),
        "metrics": metrics(bench),
        "bench": bench,
    }
    failed = sum(v for k, v in entry["metrics"].items() if k.endswith(".failed"))
    with open(args.history, "a") as f:
        f.write(json.dumps(entry, sort_keys=True) + "\n")
    print(f"recorded {entry['commit'][:10]}: {len(entry['metrics'])} metrics, {failed} failed benign requests")


def load(path):
    return [json.loads(line) for line in Path(path).read_text().splitlines() if line.strip()]


def by_commit(entries):
    """Commit -> {metric: median over its runs}, in first-recorded order."""
    runs = {}
    for e in entries:
        runs.setdefault(e["commit"], []).append(e["metrics"])
    return {c: {k: statistics.median(r[k] for r in rs if k in r) for k in set().union(*rs)} for c, rs in runs.items()}


def same_machine(e):
    return e["machine"].get("cpu"), e["machine"].get("cores")


def compare(args):
    entries = load(args.history)
    if entries and not getattr(args, "any_machine", False):
        # Hosted runners vary in CPU model; compare like with like.
        entries = [e for e in entries if same_machine(e) == same_machine(entries[-1])]
    commits = by_commit(entries)
    if len(commits) < 2:
        print("compare: need runs of at least two commits on this machine type")
        return 0
    *older, newest = commits.items()
    base = older[-args.baseline:]
    rows, worse = [], []
    for key, new in sorted(newest[1].items()):
        higher_better = DIRECTION.get(key.rsplit(".", 1)[1])
        values = [m[key] for _, m in base if key in m]
        if higher_better is None or not values:
            continue
        median = statistics.median(values)
        change = (new - median) / median if median else 0.0
        bad = (-change if higher_better else change) > args.threshold
        rows.append((key, median, min(values), max(values), new, change, bad))
        if bad:
            worse.append(key)
    print(f"{newest[0][:10]} against the median of {len(base)} earlier commit(s); "
          f"threshold {args.threshold:.0%}\n")
    print("| metric | baseline median | baseline range | new | change | |\n|---|---:|---:|---:|---:|---|")
    for key, median, lo, hi, new, change, bad in rows:
        print(f"| {key} | {median:g} | {lo:g}–{hi:g} | {new:g} | {change:+.1%} | {'**worse**' if bad else ''} |")
    print(f"\n{len(worse)} metric(s) worse by more than {args.threshold:.0%}" + (f": {', '.join(worse)}" if worse else ""))
    return 1 if worse else 0


def chart(args):
    commits = list(by_commit(load(args.history)).items())
    keys = sorted({k for _, m in commits for k in m if k.rsplit(".", 1)[1] in DIRECTION})
    w, h, pad = 360, 90, 4
    parts = ["<!doctype html><meta charset=utf-8><title>nanovm benchmark history</title>",
             "<style>body{font:14px system-ui;margin:16px}div{display:inline-block;margin:8px}"
             "svg{border:1px solid #ccc}polyline{fill:none;stroke:#2563eb;stroke-width:2}</style>",
             f"<h1>nanovm bench --quick, {len(commits)} commits</h1>"]
    for key in keys:
        pts = [(i, m[key]) for i, (_, m) in enumerate(commits) if key in m]
        if not pts:
            continue
        lo, hi = min(v for _, v in pts), max(v for _, v in pts)
        span, n = (hi - lo) or 1, max(len(commits) - 1, 1)
        xy = " ".join(f"{pad + i * (w - 2 * pad) / n:.1f},{h - pad - (v - lo) * (h - 2 * pad) / span:.1f}" for i, v in pts)
        better = "higher" if DIRECTION[key.rsplit('.', 1)[1]] else "lower"
        parts.append(f"<div><b>{html.escape(key)}</b> ({better} is better)<br>"
                     f"<svg width={w} height={h}><polyline points='{xy}'/></svg><br>"
                     f"{lo:g} – {hi:g}, last {pts[-1][1]:g}</div>")
    parts.append("<p>Commits, oldest first: " + ", ".join(c[:10] for c, _ in commits) + "</p>")
    Path(args.out).write_text("\n".join(parts) + "\n")
    print(f"wrote {args.out}: {len(keys)} metrics")


def export(args):
    for e in load(args.history):
        env = f"{e['machine']['cpu']} x{e['machine']['cores']} {e['profile']} {e['mode']}"
        for name in RUNS:
            m = e["metrics"]
            if f"{name}.requests" not in m or f"{name}.elapsed_ms" not in m:
                continue
            print(json.dumps({
                "run": f"{e['commit'][:12]}@{e['recorded']}", "variant": e["commit"][:12], "workload": name,
                "path": "in-process", "environment": env, "provenance": "measured",
                "requests": m[f"{name}.requests"], "errors": m.get(f"{name}.failed", 0),
                "seconds": m[f"{name}.elapsed_ms"] / 1000, "p99_ms": m.get(f"{name}.p99_ms"),
                "expected_spans": 0, "received_spans": 0,
            }))


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("record")
    r.add_argument("--history", required=True)
    r.add_argument("--bench-json", help="record this bench.rs JSON instead of running the bench")
    r.add_argument("--profile", choices=["release", "dev"], default="release")
    c = sub.add_parser("compare")
    c.add_argument("--history", required=True)
    c.add_argument("--threshold", type=float, default=0.05)
    c.add_argument("--baseline", type=int, default=5, help="earlier commits whose median is the baseline")
    c.add_argument("--any-machine", action="store_true", help="also compare runs on other CPU models and core counts")
    g = sub.add_parser("chart")
    g.add_argument("--history", required=True)
    g.add_argument("--out", required=True)
    x = sub.add_parser("export")
    x.add_argument("--history", required=True)
    args = p.parse_args()
    sys.exit({"record": record, "compare": compare, "chart": chart, "export": export}[args.cmd](args) or 0)


if __name__ == "__main__":
    main()
