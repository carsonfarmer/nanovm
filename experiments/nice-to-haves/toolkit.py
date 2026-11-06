#!/usr/bin/env python3
"""Offline, standard-library-only developer tools. See README.md for contracts."""

import argparse
import hashlib
import json
import math
from pathlib import Path
import re
import statistics
import sys
from urllib.parse import unquote, urlsplit


def number(value, name, minimum=0, positive=False):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError(f"{name}: expected a number")
    if not math.isfinite(value) or value < minimum or (positive and value == 0):
        raise ValueError(f"{name}: invalid finite range")
    return value


def capacity(data):
    """Linear budget estimate; provenance is mandatory for every scalar."""
    names = {
        "application_cores", "collector_cores", "target_utilization",
        "application_cpu_us", "collector_cpu_us", "memory_budget_mib",
        "fixed_memory_mib", "resident_object_kib", "target_rps", "mean_latency_ms",
    }
    if set(data) != names:
        raise ValueError(f"capacity fields must be exactly: {', '.join(sorted(names))}")
    values = {}
    for key, item in data.items():
        if not isinstance(item, dict) or set(item) != {"value", "basis", "source"}:
            raise ValueError(f"{key}: require value, basis, source")
        if item["basis"] not in ("measured", "assumed"):
            raise ValueError(f"{key}: basis must be measured or assumed")
        if not isinstance(item["source"], str) or not item["source"].strip():
            raise ValueError(f"{key}: provide evidence or explain assumption")
        values[key] = number(item["value"], key)
    v = values
    for key in ("application_cpu_us", "collector_cpu_us", "resident_object_kib"):
        number(v[key], key, positive=True)
    if not 0 < v["target_utilization"] <= 1:
        raise ValueError("target_utilization must be in (0, 1]")
    if v["fixed_memory_mib"] > v["memory_budget_mib"]:
        raise ValueError("fixed memory exceeds budget")
    app = v["application_cores"] * v["target_utilization"] * 1e6 / v["application_cpu_us"]
    collector = v["collector_cores"] * v["target_utilization"] * 1e6 / v["collector_cpu_us"]
    return {
        "inputs": data,
        "model": "linear CPU and resident-memory estimate, not a safe production limit",
        "application_rps_ceiling": app,
        "collector_rps_ceiling": collector,
        "combined_rps_ceiling": min(app, collector),
        "target_within_cpu_estimate": v["target_rps"] <= min(app, collector),
        "resident_objects_estimate": math.floor(
            (v["memory_budget_mib"] - v["fixed_memory_mib"]) * 1024 / v["resident_object_kib"]
        ),
        "mean_inflight_at_target": v["target_rps"] * v["mean_latency_ms"] / 1000,
        "warnings": [
            "Application and Collector cores must be separately budgeted, not shared SMT siblings.",
            "CPU microseconds are per request including all expected spans, not latency or per-span CPU.",
            "Memory density is an observed workload slope, not a hard object limit or memory reservation.",
            "Queueing, hot objects, I/O, network, burstiness, and cgroup limits are not modeled.",
        ],
    }


def summarize(rows):
    """Pool throughput by elapsed time; never average percentiles."""
    groups = {}
    seen = set()
    fields = {
        "run", "variant", "workload", "path", "environment", "provenance",
        "requests", "errors", "seconds", "p99_ms", "expected_spans", "received_spans",
    }
    for row in rows:
        if set(row) != fields:
            raise ValueError(f"benchmark row fields must be exactly: {', '.join(sorted(fields))}")
        for name in ("run", "variant", "workload", "path", "environment", "provenance"):
            if not isinstance(row[name], str) or not row[name].strip():
                raise ValueError(f"{name}: expected nonempty string")
        if row["path"] not in ("in-process", "http-node", "http-proxy"):
            raise ValueError("path must be in-process, http-node, or http-proxy")
        if row["provenance"] not in ("measured", "synthetic"):
            raise ValueError("provenance must be measured or synthetic")
        for name in ("requests", "errors", "expected_spans", "received_spans"):
            if type(row[name]) is not int or row[name] < 0:
                raise ValueError(f"{name}: expected nonnegative integer")
        number(row["seconds"], "seconds", positive=True)
        if row["p99_ms"] is not None:
            number(row["p99_ms"], "p99_ms")
        if row["errors"] > row["requests"]:
            raise ValueError("errors exceed requests")
        key = tuple(row[n] for n in ("variant", "workload", "path", "environment", "provenance"))
        if (key, row["run"]) in seen:
            raise ValueError(f"duplicate run: {row['run']}")
        seen.add((key, row["run"]))
        groups.setdefault(key, []).append(row)
    if not groups:
        raise ValueError("no benchmark rows")
    result = []
    for key, runs in sorted(groups.items()):
        count = sum(r["requests"] for r in runs)
        seconds = sum(r["seconds"] for r in runs)
        errors = sum(r["errors"] for r in runs)
        percentiles = [r["p99_ms"] for r in runs if r["p99_ms"] is not None]
        result.append(dict(
            zip(("variant", "workload", "path", "environment", "provenance"), key),
            runs=len(runs), requests=count, errors=errors,
            aggregate_rps=count / seconds, successful_rps=(count - errors) / seconds,
            median_run_rps=statistics.median(r["requests"] / r["seconds"] for r in runs),
            run_p99_ms_range=[min(percentiles), max(percentiles)] if percentiles else None,
            runs_without_p99=len(runs) - len(percentiles),
            expected_spans=sum(r["expected_spans"] for r in runs),
            received_spans=sum(r["received_spans"] for r in runs),
            every_run_span_count_matches=all(r["expected_spans"] == r["received_spans"] for r in runs),
        ))
    return {"groups": result, "notes": [
        "Percentiles cannot be pooled from p99 summaries; range is across run p99s.",
        "Equal span counts do not prove unique delivery; duplicates can hide missing spans.",
        "No significance, performance clearance, or cross-environment comparison is inferred.",
    ]}


def u32(data, offset):
    value = 0
    for i in range(5):
        if offset >= len(data):
            raise ValueError("truncated unsigned LEB128")
        byte = data[offset]
        offset += 1
        if i == 4 and byte > 15:
            raise ValueError("unsigned LEB128 exceeds u32")
        value |= (byte & 127) << (i * 7)
        if not byte & 128:
            return value, offset
    raise ValueError("unterminated unsigned LEB128")


def inspect_wasm(data):
    """Inspect top-level binary envelope only; no component/WIT validation."""
    versions = {b"\x01\x00\x00\x00": "core-module", b"\x0d\x00\x01\x00": "component"}
    if len(data) < 8 or data[:4] != b"\x00asm" or data[4:8] not in versions:
        raise ValueError("unsupported Wasm header")
    sections = []
    offset = 8
    while offset < len(data):
        start = offset
        section_id = data[offset]
        size, offset = u32(data, offset + 1)
        end = offset + size
        if end > len(data):
            raise ValueError(f"section at {start} exceeds file length")
        section = {"id": section_id, "offset": start, "payload_bytes": size, "total_bytes": end - start}
        if section_id == 0:
            payload = data[offset:end]
            length, name_start = u32(payload, 0)
            if name_start + length > len(payload):
                raise ValueError("custom section name exceeds section")
            section["custom_name"] = payload[name_start:name_start + length].decode("utf-8")
        sections.append(section)
        offset = end
    return {
        "kind": versions[data[4:8]], "bytes": len(data),
        "sha256": hashlib.sha256(data).hexdigest(), "sections": sections,
        "custom_section_bytes": sum(s["total_bytes"] for s in sections if s["id"] == 0),
        "scope": "top-level envelope only; not semantic validation, import analysis, or recursive size attribution",
    }


def check_links(root, documents):
    """Check local file targets of simple inline Markdown links, never fetch URLs."""
    root = root.resolve()
    problems, checked, skipped = [], 0, 0
    for document in documents:
        document = document.resolve()
        if not document.is_relative_to(root):
            raise ValueError(f"document outside root: {document}")
        fenced = False
        fence_char, fence_size = "", 0
        for line_no, line in enumerate(document.read_text().splitlines(), 1):
            fence = re.match(r"^\s{0,3}(`{3,}|~{3,})", line)
            if fence:
                mark = fence[1]
                if not fenced:
                    fenced, fence_char, fence_size = True, mark[0], len(mark)
                elif mark[0] == fence_char and len(mark) >= fence_size:
                    fenced = False
                continue
            if fenced:
                continue
            # Deliberately limited grammar, not a CommonMark parser.
            for match in re.finditer(r"!?\[[^\]\n]*\]\((<[^>]+>|[^\s()]+)(?:\s+\"[^\"]*\")?\)", line):
                target = match[1].strip("<>")
                parsed = urlsplit(target)
                if parsed.scheme or parsed.netloc or not parsed.path:
                    skipped += 1
                    continue
                resolved = (document.parent / unquote(parsed.path)).resolve()
                checked += 1
                reason = None
                if not resolved.is_relative_to(root):
                    reason = "target escapes root (including symlinks)"
                elif not resolved.exists():
                    reason = "missing file target"
                if reason:
                    problems.append({"document": str(document.relative_to(root)), "line": line_no,
                                     "target": target, "reason": reason})
    return {"checked": checked, "skipped_external_or_anchor_only": skipped, "problems": problems,
            "scope": "simple inline file links only; fragments, references, HTML and external URLs not validated"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    for name in ("capacity", "summarize", "wasm"):
        command = commands.add_parser(name)
        command.add_argument("input", type=Path)
    links = commands.add_parser("links")
    links.add_argument("--root", type=Path, required=True)
    links.add_argument("documents", type=Path, nargs="+")
    args = parser.parse_args()
    try:
        if args.command == "capacity":
            result = capacity(json.loads(args.input.read_text()))
        elif args.command == "summarize":
            result = summarize(json.loads(line) for line in args.input.read_text().splitlines() if line.strip())
        elif args.command == "wasm":
            result = inspect_wasm(args.input.read_bytes())
        else:
            result = check_links(args.root, args.documents)
        print(json.dumps(result, indent=2, allow_nan=False))
        return 1 if result.get("problems") else 0
    except (OSError, ValueError, TypeError, KeyError) as error:
        parser.exit(2, f"error: {error}\n")


if __name__ == "__main__":
    sys.exit(main())
