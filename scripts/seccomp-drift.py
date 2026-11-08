#!/usr/bin/env python3
"""Seccomp drift check: what jailed workers really call, against jail.rs.

Runs the integration tests that start jailed workers under `strace -f`, keeps
only the syscalls worker threads made after the seccomp filter applied to
them, and compares those with the allowlist in `crates/nanovm/src/jail.rs`.

Drift is a jailed syscall the filter refused (it returned ENOSYS): either a
syscall missing from the allowlist, or one allowed only for some arguments,
called with others. A dependency upgrade that starts needing a new syscall
shows up here instead of as a mysterious failure. Allowlisted syscalls never
seen are listed too, as candidates to review (some are allowed on purpose
for `wasi:filesystem` calls the tests don't make).

    python3 scripts/seccomp-drift.py                  # build, trace, report
    python3 scripts/seccomp-drift.py --trace FILE     # report on a saved trace
    python3 scripts/seccomp-drift.py --json out.json --fail-on-drift

Exit status: 0 no drift (or drift without --fail-on-drift), 1 drift with
--fail-on-drift, 2 the check itself failed (no worker traced, tests failed to
build, a parser inconsistency).
"""

import argparse
import collections
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent
JAIL = ROOT / "crates/nanovm/src/jail.rs"

# Integration tests that start jailed workers (`supervised*`, the proxy's
# `front`, or the nanovm binary). Names are substrings, as libtest filters.
JAIL_TESTS = [
    "supervised_workers_contain_attacks",
    "killed_worker_is_restarted_and_keeps_its_volumes",
    "kv_works_in_a_jailed_worker",
    "objects_call_objects",
    "a_crash_mid_request_is_rolled_back",
    "worker_connection_outlives_many_requests",
    "open_response_bodies_count_against_outbound_concurrency",
    "registry_app_switches_versions_after_a_drain",
    "a_drain_that_runs_out_of_time_kills_the_old_worker",
    "a_switch_finishes_when_its_request_goes_away",
    "a_running_version_is_checked_against_each_policy",
    "a_node_runs_at_most_max_apps",
    "jailed_workers_export_otel_without_guest_network_access",
    "front_door_events_cover_failures_and_link_to_worker",
    "websockets_reach_their_object_and_fan_out_within_it",
    "websocket_limits_hold",
    "http_front_end",
]

# Refused on purpose, and handled by the caller: glibc's thread creation
# falls back from clone3 to clone, whose flags the filter can check.
EXPECTED_DENIALS = {"clone3"}


def allowlist(source, arch):
    """The syscall names `seccomp_filter` allows on `arch`, and those of them
    allowed only for some arguments."""
    body = source.split("fn seccomp_filter", 1)[1]
    body = body.split("\n}\n", 1)[0]
    allowed, by_args = set(), set()
    # `#[cfg(target_arch = "x86_64")] allow.extend([...]);` blocks.
    for cfg_arch, block in re.findall(r'#\[cfg\(target_arch = "(\w+)"\)\]\s*allow\.extend\(\[(.*?)\]\);', body, re.S):
        names = set(re.findall(r"libc::SYS_(\w+)", block))
        if cfg_arch == arch:
            allowed |= names
        body = body.replace(block, "")
    allowed |= set(re.findall(r"libc::SYS_(\w+)", body))
    by_args |= set(re.findall(r"rules\.insert\(\s*libc::SYS_(\w+)", body))
    if not allowed:
        raise SystemExit("no allowlist found in jail.rs")
    return allowed, by_args


LINE = re.compile(r"^(\d+)\s+(.*)$")
START = re.compile(r"^([a-z_0-9]+)\((.*)$")
RESUMED = re.compile(r"^<\.\.\. ([a-z_0-9]+) resumed>(.*)$")
RESULT = re.compile(r"\)\s+=\s+(-?\d+|\?|0x[0-9a-f]+)(?:\s+(E[A-Z0-9]+))?")


def calls(lines):
    """Complete syscalls from `strace -f` output, in order of completion:
    (index, tid, name, text, result, errno). `index` is where the call
    started, so a thread's position relative to others' events is kept."""
    pending = {}
    for index, raw in enumerate(lines):
        m = LINE.match(raw)
        if not m:
            continue
        tid, rest = int(m.group(1)), m.group(2)
        if rest.startswith(("+++", "---")):
            continue
        r = RESUMED.match(rest)
        if r:
            start = pending.pop(tid, None)
            if start is None or start[1] != r.group(1):
                continue
            began, name, text = start
            text += r.group(2)
        else:
            s = START.match(rest)
            if not s:
                continue
            began, name, text = index, s.group(1), s.group(2)
            if text.endswith("<unfinished ...>"):
                pending[tid] = (began, name, text[: -len("<unfinished ...>")].rstrip())
                continue
        res = RESULT.search(text)
        result = res.group(1) if res else "?"
        yield began, tid, name, text, result, (res.group(2) if res else None)


def analyse(lines, allowed, by_args):
    """What jailed worker threads called, from one `strace -f` trace."""
    log = list(calls(lines))
    creator, thread_of, born = {}, {}, {}
    exec_worker, seccomp_at = {}, {}
    for index, tid, name, text, result, _ in log:
        if name in ("clone", "clone3", "fork", "vfork") and result.isdigit() and int(result) > 0:
            child = int(result)
            creator[child] = tid
            thread_of[child] = "CLONE_THREAD" in text
            born[child] = index
        elif name == "execve" and result == "0":
            argv = re.search(r"\[(.*?)\]", text)
            words = re.findall(r'"([^"]*)"', argv.group(1)) if argv else []
            if len(words) >= 2 and words[0].endswith("nanovm") and words[1] == "worker":
                exec_worker[tid] = index
        elif name == "seccomp" and result == "0" and "SECCOMP_SET_MODE_FILTER" in text:
            seccomp_at.setdefault(tid, index)

    def leader(tid):
        while thread_of.get(tid):
            tid = creator[tid]
        return tid

    jailed_cache = {}

    def jailed_from(tid):
        """The trace index from which `tid` runs under the filter."""
        if tid in jailed_cache:
            return jailed_cache[tid]
        at = seccomp_at.get(tid, float("inf"))
        parent = creator.get(tid)
        if parent is not None and thread_of.get(tid) and jailed_from(parent) <= born[tid]:
            at = min(at, born[tid])
        jailed_cache[tid] = at
        return at

    workers = {tid for tid in exec_worker}
    counts = collections.Counter()
    denied = collections.defaultdict(list)
    escaped = collections.defaultdict(list)
    unjailed = collections.Counter()
    worker_threads = set()
    for index, tid, name, text, result, errno in log:
        proc = leader(tid)
        if proc not in workers or index < exec_worker[proc]:
            continue
        worker_threads.add(tid)
        if index <= jailed_from(tid):
            # After its process jailed itself, every thread should be jailed.
            if seccomp_at.get(proc, float("inf")) < index:
                unjailed[name] += 1
            continue
        counts[name] += 1
        if errno == "ENOSYS":
            denied[name].append(text[:160])
        elif name not in allowed:
            escaped[name].append(text[:160])

    drift = {
        n: {"count": len(v), "allowlisted_with_argument_rules": n in by_args, "examples": sorted(set(v))[:5]}
        for n, v in sorted(denied.items()) if n not in EXPECTED_DENIALS
    }
    return {
        "workers": len(workers),
        "jailed_workers": sum(1 for w in workers if w in seccomp_at),
        "worker_threads": len(worker_threads),
        "jailed_syscalls": sum(counts.values()),
        "observed": dict(sorted(counts.items())),
        "drift": drift,
        "expected_denials": {n: len(v) for n, v in sorted(denied.items()) if n in EXPECTED_DENIALS},
        "unused_allowlist": sorted(allowed - set(counts)),
        # A jailed thread can't make a call the filter refuses succeed, and
        # no worker thread should run outside the jail once it is entered.
        "inconsistent": {n: sorted(set(v))[:5] for n, v in sorted(escaped.items())},
        "unjailed_after_jail": dict(sorted(unjailed.items())),
    }


def build_tests():
    out = subprocess.run(
        ["cargo", "test", "--locked", "-p", "nanovm", "--test", "integration", "--no-run", "--message-format=json"],
        cwd=ROOT, check=True, stdout=subprocess.PIPE, text=True,
    ).stdout
    for line in out.splitlines():
        msg = json.loads(line)
        if msg.get("reason") == "compiler-artifact" and msg["target"]["name"] == "integration" and msg.get("executable"):
            return msg["executable"]
    raise SystemExit("no integration test executable")


def trace(tests, out):
    exe = build_tests()
    cmd = ["strace", "-f", "-qq", "-e", "signal=none", "-s", "48", "-o", str(out),
           exe, "--test-threads=1", *tests]
    env = dict(os.environ, RUST_TEST_THREADS="1")
    env.pop("RUST_BACKTRACE", None)
    run = subprocess.run(cmd, cwd=ROOT / "crates/nanovm", env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    summary = [l for l in run.stdout.splitlines() if l.startswith(("test ", "test result"))]
    return run.returncode, summary


def report(result, tests_status):
    lines = [
        f"Traced {result['workers']} jailed worker processes ({result['worker_threads']} threads), "
        f"{result['jailed_syscalls']} syscalls under the filter, {len(result['observed'])} distinct.",
    ]
    if tests_status:
        lines.append(f"Traced tests: {tests_status}")
    if result["drift"]:
        lines.append("DRIFT: jailed workers called syscalls the filter refused (ENOSYS):")
        for name, d in result["drift"].items():
            why = "arguments outside its rule" if d["allowlisted_with_argument_rules"] else "not allowlisted"
            lines.append(f"  {name}: {d['count']} times, {why}; e.g. {d['examples'][0]}")
    else:
        lines.append("No drift: every jailed syscall was allowed, apart from expected denials "
                     f"{result['expected_denials'] or '{}'}.")
    if result["unused_allowlist"]:
        lines.append(f"Allowlisted but not seen ({len(result['unused_allowlist'])}): {', '.join(result['unused_allowlist'])}")
    for key in ("inconsistent", "unjailed_after_jail"):
        if result[key]:
            lines.append(f"{key.upper()}: {result[key]}")
    return "\n".join(lines)


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--trace", type=Path, help="analyse this saved `strace -f` output instead of running tests")
    p.add_argument("--keep", type=Path, help="where to save the trace (default: a temporary file)")
    p.add_argument("--json", type=Path, help="also write the full result here")
    p.add_argument("--fail-on-drift", action="store_true")
    p.add_argument("tests", nargs="*", help="test name filters (default: the tests that start jailed workers)")
    args = p.parse_args()

    arch = {"aarch64": "aarch64", "arm64": "aarch64"}.get(platform.machine(), platform.machine())
    allowed, by_args = allowlist(JAIL.read_text(), arch)
    tests_status = None
    if args.trace:
        path = args.trace
    else:
        path = args.keep or Path(tempfile.mkstemp(prefix="seccomp-drift-", suffix=".strace")[1])
        code, summary = trace(args.tests or JAIL_TESTS, path)
        tests_status = next((l for l in summary if l.startswith("test result")), f"exit {code}")
        failed = [l for l in summary if l.endswith("FAILED")]
        if failed:
            tests_status += "; failed under strace: " + ", ".join(l.split()[1] for l in failed)
    with open(path, errors="replace") as f:
        result = analyse(f, allowed, by_args)
    if not args.trace and not args.keep:
        path.unlink()
    result["tests"] = tests_status
    if args.json:
        args.json.write_text(json.dumps(result, indent=2) + "\n")
    text = report(result, tests_status)
    print(text)
    if os.environ.get("GITHUB_ACTIONS") == "true":
        for name, d in result["drift"].items():
            print(f"::warning title=seccomp drift::jailed workers called {name} {d['count']} times and the filter refused it")
        if os.environ.get("GITHUB_STEP_SUMMARY"):
            with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as f:
                f.write("## Seccomp drift\n\n```\n" + text + "\n```\n")
    if result["workers"] == 0 or result["jailed_workers"] == 0 or result["inconsistent"] or result["unjailed_after_jail"]:
        return 2
    return 1 if result["drift"] and args.fail_on_drift else 0


if __name__ == "__main__":
    sys.exit(main())
