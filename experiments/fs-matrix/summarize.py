"""Median and range per filesystem from run.sh's results.jsonl, as Markdown.
Usage: python3 -I summarize.py results.jsonl"""
import json, statistics, sys

rows = [json.loads(line) for line in open(sys.argv[1]) if line.strip()]
cols = [
    ("fdatasync 4 KiB p50 ms", lambda r: r["fsync"]["overwrite_fdatasync_ms"]["p50"]),
    ("atomic write+rename p50 ms", lambda r: r["fsync"]["atomic_write_rename_ms"]["p50"]),
    ("atomic p99 ms", lambda r: r["fsync"]["atomic_write_rename_ms"]["p99"]),
    ("/incr serial /s", lambda r: r["incr_serial"]["rps"]),
    ("serial p50 ms", lambda r: r["incr_serial"]["p50_ms"]),
    ("/incr c64 /s", lambda r: r["incr_c64"]["rps"]),
    ("c64 p99 ms", lambda r: r["incr_c64"]["p99_ms"]),
]
print("| filesystem | rounds | " + " | ".join(c for c, _ in cols) + " |")
print("|---|---:|" + "---:|" * len(cols))
for fs in sorted({r["fs"] for r in rows}):
    rs = [r for r in rows if r["fs"] == fs]
    cells = []
    for _, get in cols:
        v = [get(r) for r in rs]
        m = statistics.median(v)
        cells.append(f"{m:g} ({min(v):g}–{max(v):g})" if len(v) > 1 else f"{m:g}")
    print(f"| {fs} | {len(rs)} | " + " | ".join(cells) + " |")
failed = sum(r[k]["n"] - r[k]["ok"] for r in rows for k in ("incr_serial", "incr_c64"))
print(f"\nMedian (min–max) over rounds. Non-200 responses: {failed}. "
      f"Load average during runs {min(r['load1'] for r in rows):g}–{max(r['load1'] for r in rows):g}.")
