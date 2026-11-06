#!/usr/bin/env python3
"""Deterministic public transcripts, compatible histogram pooling and CPU topology."""

import argparse
import json
import os
from pathlib import Path
import subprocess

from toolkit import number, summarize


def transcript(rows):
    # Sorting before floating-point addition also makes reversed input reproducible.
    rows = sorted(rows, key=lambda row: json.dumps(row, sort_keys=True, allow_nan=False))
    groups = summarize(rows)["groups"]
    lines = ["# Benchmark transcript", "", "Public summary: workload/variant/environment/run labels omitted.",
             "No performance clearance or unique-delivery guarantee is inferred.", "",
             "| Group | Basis | Path | Runs | Requests | Errors | Successful/s | Run p99 range (ms) | Missing p99 | Span counts match each run |",
             "| --- | --- | --- | ---: | ---: | ---: | ---: | --- | ---: | --- |"]
    for index, group in enumerate(groups, 1):
        latency = group["run_p99_ms_range"]
        shown = "unavailable" if latency is None else f"{latency[0]:.6g}–{latency[1]:.6g}"
        lines.append(f"| {index} | {group['provenance']} | {group['path']} | {group['runs']} | "
                     f"{group['requests']} | {group['errors']} | {group['successful_rps']:.6g} | "
                     f"{shown} | {group['runs_without_p99']} | {str(group['every_run_span_count_matches']).lower()} |")
    return "\n".join(lines) + "\n"


def merge_histograms(rows):
    if not isinstance(rows, list) or not rows:
        raise ValueError("provide a nonempty list of histograms")
    fields = {"run", "unit", "bounds", "counts", "scope", "provenance"}
    seen, combined, signature = set(), None, None
    for row in rows:
        if set(row) != fields:
            raise ValueError("histogram fields must be run, unit, bounds, counts, scope, provenance")
        if not isinstance(row["run"], str) or not row["run"] or row["run"] in seen:
            raise ValueError("each run needs a unique nonempty ID")
        seen.add(row["run"])
        if row["unit"] not in ("ms", "s") or row["provenance"] not in ("synthetic", "measured"):
            raise ValueError("invalid unit or provenance")
        if not isinstance(row["scope"], str) or not row["scope"]:
            raise ValueError("scope must identify the same population and sampling policy")
        bounds, counts = row["bounds"], row["counts"]
        if not isinstance(bounds, list) or not isinstance(counts, list) or len(counts) != len(bounds) + 1:
            raise ValueError("counts must include exactly one overflow bin")
        for bound in bounds:
            number(bound, "bound")
        if any(a >= b for a, b in zip(bounds, bounds[1:])):
            raise ValueError("bounds must strictly increase")
        if any(type(count) is not int or count < 0 for count in counts):
            raise ValueError("counts must be nonnegative disjoint-bin integers, not cumulative buckets")
        current = (row["unit"], bounds, row["scope"], row["provenance"])
        if signature is not None and signature != current:
            raise ValueError("incompatible unit, boundaries, population or provenance")
        signature = current
        combined = counts.copy() if combined is None else [a + b for a, b in zip(combined, counts)]
    total = sum(combined)
    quantiles = {}
    for name, numerator in (("p50", 50), ("p95", 95), ("p99", 99)):
        if not total:
            quantiles[name] = None
            continue
        rank, cumulative = (numerator * total + 99) // 100, 0
        for index, count in enumerate(combined):
            cumulative += count
            if cumulative >= rank:
                quantiles[name] = {"lower": bounds[index - 1] if index else 0,
                                   "lower_inclusive": index == 0,
                                   "upper_inclusive": bounds[index] if index < len(bounds) else None}
                break
    return {"unit": signature[0], "bounds": signature[1], "counts": combined,
            "scope": signature[2], "provenance": signature[3], "runs": len(seen), "samples": total,
            "quantile_intervals": quantiles, "note": "null upper means unbounded overflow; no within-bin interpolation"}


def topology(document, affinity, application=(), collector=()):
    cpus, groups = {}, {}
    for item in document["cpus"]:
        def integer(key):
            value = item[key]
            if isinstance(value, bool) or not str(value).isdigit():
                raise ValueError(f"unknown or invalid {key}; cannot establish physical topology")
            return int(value)
        cpu, core, socket = integer("cpu"), integer("core"), integer("socket")
        if cpu in cpus:
            raise ValueError("duplicate CPU")
        online = item["online"]
        if online in (True, "yes", "y"):
            online = True
        elif online in (False, "no", "n"):
            online = False
        else:
            raise ValueError("unknown online status")
        cpus[cpu] = {"cpu": cpu, "core": core, "socket": socket, "online": online}
        if online and cpu in affinity:
            groups.setdefault((socket, core), []).append(cpu)
    if not cpus or not set(affinity).issubset(cpus):
        raise ValueError("empty topology or unknown affinity CPU")
    eligible = {cpu for values in groups.values() for cpu in values}
    if not set(application).issubset(eligible) or not set(collector).issubset(eligible):
        raise ValueError("requested allocation contains unavailable CPU")
    app_cores = {(cpus[cpu]["socket"], cpus[cpu]["core"]) for cpu in application}
    col_cores = {(cpus[cpu]["socket"], cpus[cpu]["core"]) for cpu in collector}
    return {"eligible_logical_cpus": sorted(eligible), "physical_core_groups": [
        {"socket": socket, "core": core, "cpus": sorted(values)} for (socket, core), values in sorted(groups.items())],
        "application": sorted(set(application)), "collector": sorted(set(collector)),
        "shared_physical_cores": [list(pair) for pair in sorted(app_cores & col_cores)],
        "cpu_isolation_established": bool(application and collector) and not bool(app_cores & col_cores),
        "note": "reported topology and affinity only; virtualized topology, cgroup budgets and actual pinning are not verified"}


def cpu_list(text):
    if not text:
        return []
    if any(not part.isdigit() for part in text.split(",")):
        raise argparse.ArgumentTypeError("use comma-separated nonnegative CPU IDs")
    return [int(part) for part in text.split(",")]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    for name in ("transcript", "histograms"):
        commands.add_parser(name).add_argument("input", type=Path)
    topo = commands.add_parser("topology")
    topo.add_argument("--input", type=Path, help="saved lscpu --json --extended=CPU,CORE,SOCKET,ONLINE output")
    topo.add_argument("--affinity", type=cpu_list, help="required with saved input; otherwise current process affinity")
    topo.add_argument("--application", type=cpu_list, default=[])
    topo.add_argument("--collector", type=cpu_list, default=[])
    args = parser.parse_args()
    try:
        if args.command == "transcript":
            print(transcript([json.loads(line) for line in args.input.read_text().splitlines() if line.strip()]), end="")
            return
        if args.command == "histograms":
            result = merge_histograms(json.loads(args.input.read_text()))
        else:
            if args.input:
                if args.affinity is None:
                    raise ValueError("saved topology requires explicit affinity")
                data = json.loads(args.input.read_text())
            else:
                data = json.loads(subprocess.run(["lscpu", "--json", "--extended=CPU,CORE,SOCKET,ONLINE"],
                                                check=True, capture_output=True, timeout=5).stdout)
            affinity = args.affinity if args.affinity is not None else sorted(os.sched_getaffinity(0))
            result = topology(data, affinity, args.application, args.collector)
        print(json.dumps(result, indent=2, allow_nan=False))
    except (ValueError, KeyError, TypeError, OSError, AttributeError, subprocess.SubprocessError) as error:
        parser.exit(2, f"analysis input failure: {type(error).__name__}\n")


if __name__ == "__main__":
    main()
