#!/usr/bin/env python3
"""Checks a host against a node config before `nanovm serve` meets it.

    deploy/single-node/check-host.py /etc/nanovm/nanovm.toml [--nofile N]

For the config's `max_live_objects` it prints what each app's worker needs
(file descriptors and memory mappings, as `engine::preflight` computes them),
the disk an object can use at worst, and checks this host: the fd limit (the
unit's `LimitNOFILE` if systemd has one for nanovm, else this shell's hard
limit, or `--nofile`), `vm.max_map_count`, Landlock, root, the data directory
(searchable by app uids, filesystem, mount options), the cgroup directory if
set, CA certificates and RUST_BACKTRACE. Exit 1 if anything would stop the
node or its workers; warnings exit 0.
"""

import argparse
import ctypes
import os
import platform
import resource
import shutil
import stat
import subprocess
import sys
import tomllib
from pathlib import Path

# crates/nanovm/src/engine.rs
FDS_PER_OBJECT, MAPS_PER_OBJECT, SPARE_SLOTS = 2, 24, 32
DEFAULTS = {"max_live_objects": 1000, "data_dir": "nanovm-data", "memory_reservation_mb": 128, "max_apps": 100}
LIMIT_DEFAULTS = {"disk_mb": 64, "memory_mb": 64, "max_files": 1000}


def needs(cfg):
    n = cfg["max_live_objects"] + SPARE_SLOTS
    disk = cfg["limits"]["disk_mb"]
    return {
        "fds": n * FDS_PER_OBJECT + 1024,
        "maps": n * MAPS_PER_OBJECT + 4096,
        # Per object: its logical quota, and while a request runs the journal
        # and the files it replaced may each hold as much again (README).
        "disk_per_object_mb": disk,
        "disk_per_busy_object_mb": 3 * disk,
    }


def unit_nofile():
    if not shutil.which("systemctl"):
        return None
    out = subprocess.run(["systemctl", "show", "-p", "LimitNOFILE", "--value", "nanovm.service"],
                         capture_output=True, text=True)
    value = out.stdout.strip()
    return int(value) if out.returncode == 0 and value.isdigit() else None


def landlock_abi():
    """Landlock ABI version (0 when unsupported), via landlock_create_ruleset."""
    nr = {"x86_64": 444, "aarch64": 444}.get(platform.machine())
    if nr is None:
        return None
    libc = ctypes.CDLL(None, use_errno=True)
    abi = libc.syscall(nr, None, ctypes.c_size_t(0), ctypes.c_uint32(1))  # LANDLOCK_CREATE_RULESET_VERSION
    return abi if abi > 0 else 0


def mount_of(path):
    best = ("", "", "")
    for line in Path("/proc/self/mounts").read_text().splitlines():
        _, point, fstype, opts, *_ = line.split()
        point = point.replace("\\040", " ")
        if (str(path) == point or str(path).startswith(point.rstrip("/") + "/")) and len(point) >= len(best[0]):
            best = (point, fstype, opts)
    return best


def check(cfg_path, nofile=None):
    raw = tomllib.loads(Path(cfg_path).read_text())
    cfg = {**DEFAULTS, **raw}
    cfg["limits"] = {**LIMIT_DEFAULTS, **raw.get("limits", {})}
    cfg["worker"] = raw.get("worker", {})
    data = Path(cfg_path).resolve().parent / cfg["data_dir"]  # as NodeConfig::load resolves it
    need = needs(cfg)
    problems, warnings, facts = [], [], []
    facts.append(f"max_live_objects = {cfg['max_live_objects']} per app; up to max_apps = {cfg['max_apps']} workers")

    source = "--nofile"
    if nofile is None:
        nofile, source = unit_nofile(), "nanovm.service LimitNOFILE"
    if nofile is None:
        nofile, source = resource.getrlimit(resource.RLIMIT_NOFILE)[1], "this shell's hard limit"
    facts.append(f"file descriptors: each worker needs {need['fds']}; {source} is {nofile}")
    if nofile < need["fds"]:
        problems.append(f"raise the fd limit to at least {need['fds']} (LimitNOFILE=, ulimit -n) or lower max_live_objects")

    try:
        maps = int(Path("/proc/sys/vm/max_map_count").read_text())
        facts.append(f"memory mappings: each worker needs {need['maps']}; vm.max_map_count is {maps}")
        if maps < need["maps"]:
            problems.append(f"sysctl -w vm.max_map_count={need['maps']} (and persist it in /etc/sysctl.d)")
    except OSError:
        warnings.append("cannot read vm.max_map_count")

    abi = landlock_abi()
    facts.append(f"Landlock ABI: {abi}")
    if abi == 0:
        problems.append("Landlock is unavailable (Linux 5.13+ with landlock in the LSM list): workers cannot jail themselves")
    if os.geteuid() != 0:
        warnings.append("not root: if nanovm also runs without root, every app shares one uid")

    facts.append(f"disk: an object holds up to {need['disk_per_object_mb']} MiB, "
                 f"{need['disk_per_busy_object_mb']} MiB while a request runs; quotas don't cap the total")
    probe = data if data.exists() else data.parent
    if not probe.exists():
        warnings.append(f"{data} and its parent don't exist yet")
    else:
        if not data.exists():
            warnings.append(f"{data} doesn't exist yet (the node creates it)")
        for d in [probe, *probe.parents]:
            if not os.stat(d).st_mode & stat.S_IXOTH:
                problems.append(f"{d} is not searchable by others: app uids cannot reach their directories (chmod o+x)")
        point, fstype, opts = mount_of(probe.resolve())
        usage = shutil.disk_usage(probe)
        facts.append(f"data_dir on {fstype} at {point} ({opts}); {usage.free >> 30} GiB free")
        if fstype == "tmpfs":
            warnings.append("data_dir on tmpfs: nothing survives a reboot")
        if fstype == "btrfs":
            warnings.append("btrfs had the lowest concurrent durable-write rate measured (experiments/fs-matrix)")
        if "discard" in opts.split(",") and fstype in ("ext4", "xfs"):
            warnings.append("online discard cost 29-46% of concurrent durable writes measured; prefer fstrim.timer")

    if cfg.get("cgroup"):
        g = Path(cfg["cgroup"])
        wanted = [c for c, k in (("memory", "memory_mb"), ("cpu", "cpus"), ("pids", "pids")) if cfg["worker"].get(k)]
        if not g.is_dir():
            if g.parent.is_dir() and (g.parent / "cgroup.subtree_control").exists():
                warnings.append(f"{g} doesn't exist yet (nanovm-cgroup.conf's ExecStartPre makes it)")
            else:
                problems.append(f"cgroup {g} doesn't exist")
        else:
            have = (g / "cgroup.subtree_control").read_text().split()
            missing = [c for c in wanted if c not in have]
            if missing:
                problems.append(f"{g}/cgroup.subtree_control lacks {' '.join(missing)}, needed by [worker] limits")
    elif any(cfg["worker"].get(k) for k in ("memory_mb", "cpus", "pids")):
        warnings.append("[worker] limits are set but cgroup is not: they are not applied")

    if not any(Path(p).exists() for p in ("/etc/ssl/certs/ca-certificates.crt", "/etc/pki/tls/certs/ca-bundle.crt")):
        problems.append("no CA bundle found: the node refuses to start without CA certificates")
    if os.environ.get("RUST_BACKTRACE"):
        warnings.append("RUST_BACKTRACE is set in this environment: don't pass it to nanovm (warm throughput 63K/s -> 23K/s)")
    return facts, warnings, problems


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("config")
    p.add_argument("--nofile", type=int, help="the fd limit nanovm will run with")
    args = p.parse_args()
    facts, warnings, problems = check(args.config, args.nofile)
    for f in facts:
        print(f"  {f}")
    for w in warnings:
        print(f"WARN {w}")
    for x in problems:
        print(f"FAIL {x}")
    print("ok" if not problems else f"{len(problems)} problem(s)")
    sys.exit(1 if problems else 0)


if __name__ == "__main__":
    main()
