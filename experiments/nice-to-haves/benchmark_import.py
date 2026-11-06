#!/usr/bin/env python3
"""Pinned oha 1.16.0 import and allowlisted benchmark provenance capture."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import subprocess

from toolkit import number, summarize

OHA_VERSION = "oha 1.16.0"


def digest(data):
    return hashlib.sha256(data).hexdigest()


def pinned_tool(path, expected):
    path = Path(path).resolve(strict=True)
    version = subprocess.run([str(path), "--version"], check=True, capture_output=True,
                             text=True, timeout=10).stdout.strip()
    if version != expected and not version.startswith(expected + " ("):
        raise ValueError(f"expected {expected}; incompatible tool version")
    return {"version": version, "sha256": digest(path.read_bytes())}


def validate_scenario(scenario):
    fields = {"run", "variant", "workload", "path", "environment", "provenance",
              "planned_requests", "accepted_statuses", "expected_spans", "received_spans"}
    if not isinstance(scenario, dict) or set(scenario) != fields:
        raise ValueError("scenario fields must be exactly: " + ", ".join(sorted(fields)))
    for key in ("run", "variant", "workload", "environment"):
        if not isinstance(scenario[key], str) or not re.fullmatch(r"[A-Za-z0-9_.-]{1,128}", scenario[key]):
            raise ValueError(f"{key}: use a public label (letters, digits, dot, underscore, hyphen)")
    if scenario["path"] not in ("http-node", "http-proxy"):
        raise ValueError("oha runs require http-node or http-proxy path")
    if scenario["provenance"] not in ("measured", "synthetic"):
        raise ValueError("provenance must be measured or synthetic")
    for key in ("planned_requests", "expected_spans", "received_spans"):
        if type(scenario[key]) is not int or scenario[key] < 0:
            raise ValueError(f"{key}: expected nonnegative integer")
    statuses = scenario["accepted_statuses"]
    if not isinstance(statuses, list) or not statuses or any(type(x) is not int or not 100 <= x <= 599 for x in statuses):
        raise ValueError("accepted_statuses must be a nonempty list of HTTP status integers")
    if len(set(statuses)) != len(statuses):
        raise ValueError("duplicate accepted status")


def manifest(repo, raw, scenario, oha, artifacts=()):
    validate_scenario(scenario)

    def git(*args):
        return subprocess.run(["git", "-C", str(repo), *args], check=True,
                              capture_output=True, timeout=10).stdout

    tracked = git("diff", "--no-ext-diff", "--no-textconv", "--binary", "HEAD", "--")
    untracked = git("ls-files", "--others", "--exclude-standard", "-z").split(b"\0")
    files = {}
    for label, path in artifacts:
        if not re.fullmatch(r"[a-z][a-z0-9_-]{0,63}", label) or label in files:
            raise ValueError("artifact labels must be unique public lowercase identifiers")
        data = Path(path).read_bytes()
        files[label] = {"sha256": digest(data), "bytes": len(data)}
    raw_data = Path(raw).read_bytes()
    return {
        "schema": "nanovm-nice-benchmark-v1", "scenario": scenario,
        "raw": {"sha256": digest(raw_data), "bytes": len(raw_data)},
        "tool": pinned_tool(oha, OHA_VERSION), "artifacts": files,
        "source": {
            "commit": git("rev-parse", "HEAD").decode().strip(),
            "tracked_diff_sha256": digest(tracked), "tracked_dirty": bool(tracked),
            "untracked_file_count": sum(bool(x) for x in untracked),
            "scope": "tracked diff hash covers index and working tree versus HEAD; untracked content NOT captured",
        },
        "machine": {"os": platform.system(), "architecture": platform.machine(),
                    "kernel": platform.release(), "python": platform.python_version(),
                    "logical_cpus": os.cpu_count(),
                    "affinity": sorted(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else None},
        "limitations": [
            "Snapshot at manifest creation, not proof of the machine or code used for the run.",
            "No environment, command line, URL, headers, hostname, username, remote URLs or file contents emitted.",
            "Caller labels and artifact labels must not contain secrets; hashes can reveal equality.",
            "CPU topology, cgroup limits, warmup, workload shape and collector policy still need separate evidence.",
            "Reported span counts and accepted statuses are caller assertions, not verified by oha.",
        ],
    }


def import_oha(raw_bytes, metadata):
    if not isinstance(metadata, dict) or metadata.get("schema") != "nanovm-nice-benchmark-v1":
        raise ValueError("unsupported manifest schema")
    if metadata["tool"]["version"] != OHA_VERSION:
        raise ValueError("unsupported oha version")
    if metadata["raw"] != {"sha256": digest(raw_bytes), "bytes": len(raw_bytes)}:
        raise ValueError("raw output does not match manifest")
    scenario = metadata["scenario"]
    validate_scenario(scenario)
    raw = json.loads(raw_bytes)
    statuses, errors = raw["statusCodeDistribution"], raw["errorDistribution"]
    if not isinstance(statuses, dict) or not isinstance(errors, dict):
        raise ValueError("invalid oha distributions")
    for status, count in statuses.items():
        if not re.fullmatch(r"[1-5][0-9]{2}", status):
            raise ValueError("invalid HTTP status")
    for count in [*statuses.values(), *errors.values()]:
        if type(count) is not int or count < 0:
            raise ValueError("distribution counts must be nonnegative integers")
    completed = sum(statuses.values())
    transport_errors = sum(errors.values())
    total = completed + transport_errors
    if total != scenario["planned_requests"] or total == 0:
        raise ValueError("observed request count differs from planned count or is zero; preserve raw partial run")
    seconds = number(raw["summary"]["total"], "summary.total", positive=True)
    p99 = raw["latencyPercentiles"]["p99"]
    if completed:
        p99 = number(p99, "latencyPercentiles.p99") * 1000
    elif p99 is not None:
        raise ValueError("expected null latency when no HTTP response completed")
    rejected = sum(count for status, count in statuses.items() if int(status) not in scenario["accepted_statuses"])
    row = {key: scenario[key] for key in ("run", "variant", "workload", "path", "environment", "provenance",
                                        "expected_spans", "received_spans")}
    row.update(requests=total, errors=transport_errors + rejected, seconds=seconds, p99_ms=p99)
    summarize([row])  # Keep the normalized public contract aligned with batch 1.
    return row


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    capture = sub.add_parser("manifest")
    capture.add_argument("--repo", type=Path, required=True)
    capture.add_argument("--raw", type=Path, required=True)
    capture.add_argument("--scenario", type=Path, required=True)
    capture.add_argument("--oha", type=Path, required=True)
    capture.add_argument("--artifact", action="append", default=[], metavar="LABEL=PATH")
    convert = sub.add_parser("import-oha")
    convert.add_argument("--raw", type=Path, required=True)
    convert.add_argument("--manifest", type=Path, required=True)
    args = parser.parse_args()
    try:
        if args.command == "manifest":
            artifacts = []
            for item in args.artifact:
                if "=" not in item:
                    raise ValueError("artifact requires LABEL=PATH")
                artifacts.append(item.split("=", 1))
            result = manifest(args.repo, args.raw, json.loads(args.scenario.read_text()), args.oha, artifacts)
        else:
            result = import_oha(args.raw.read_bytes(), json.loads(args.manifest.read_text()))
        print(json.dumps(result, sort_keys=True, allow_nan=False))
    except (OSError, ValueError, TypeError, KeyError, subprocess.SubprocessError) as error:
        # Raw stderr, URLs, distribution error strings and paths can contain secrets.
        parser.exit(2, f"benchmark input/tool failure: {type(error).__name__}\n")


if __name__ == "__main__":
    main()
