#!/usr/bin/env python3
"""Checks a host against a node config before `nanovm serve` meets it.

    deploy/single-node/check-host.py [/etc/nanovm/nanovm.toml] [--nofile N]
    tools/doctor [nanovm.toml]          (the same)

For the config's `max_live_objects` (the defaults without a config) it prints
what each app's worker needs (file descriptors, memory mappings and address
space, as `engine::preflight` and `engine::build` size them), the disk an object
can use at worst, and checks this host: the fd limit (the unit's `LimitNOFILE`
if systemd has one for nanovm, else this shell's hard limit, or `--nofile`),
`vm.max_map_count`, `ulimit -v`, overcommit, Landlock (ABI 2 or later),
seccomp (by installing a filter in a child), root and its capabilities, the
data directory (searchable by app uids, filesystem, mount options, and a probe
of the syncs, locks, renames and links the journal makes), local capture
history against `[capture]`, cgroup v2 and the delegated cgroup if set, the
clock (source and synchronization), CA certificates and RUST_BACKTRACE.
Every warning and failure says what to do. Exit 1 if anything would stop the
node or its workers; warnings exit 0.
"""

import argparse
import ctypes
import fcntl
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
# engine::build: up to 4 memories per object, each reserved with a 64 KiB guard.
MEMORIES_PER_OBJECT, GUARD = 4, 64 << 10
# jail.rs: a root supervisor chowns app directories and kills their workers;
# a worker sets its uid and gid (deploy/systemd's CapabilityBoundingSet).
CAPS = {"CAP_CHOWN": 0, "CAP_KILL": 5, "CAP_SETGID": 6, "CAP_SETUID": 7}
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
        # Measured: with max_live_objects = 1 (33 slots, 16.5 GiB of memory
        # reservations) a worker's engine fails under `ulimit -v` 16 GiB and
        # starts under 17 GiB.
        "address_space": n * MEMORIES_PER_OBJECT * ((cfg.get("memory_reservation_mb", 128) << 20) + GUARD) + (1 << 30),
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


def landlock_findings(abi):
    """jail.rs asks for ABI 6, best effort, and refuses only an unenforced ruleset."""
    facts, warnings, problems = [f"Landlock ABI: {abi}"], [], []
    if abi is None:
        warnings.append("cannot query Landlock on this architecture: check that workers start (`worker did not start` in 502s)")
    elif abi == 0:
        problems.append("Landlock is unavailable: workers cannot jail themselves. Use Linux 5.19+ with landlock in "
                        "the LSM list (CONFIG_LSM, or lsm= on the kernel command line); in a container, allow the "
                        "landlock_* syscalls in its seccomp profile")
    elif abi == 1:
        problems.append("Landlock ABI 1 (Linux 5.13-5.18) refuses every rename between directories under a ruleset, "
                        "and the journal moves replaced files into ~journal: use Linux 5.19+ (ABI 2)")
    elif abi < 3:
        warnings.append(f"Landlock ABI {abi}: truncation by path is confined only from ABI 3 (Linux 6.2); the uid and "
                        "seccomp still apply. Upgrade the kernel when you can")
    return facts, warnings, problems


def seccomp_probe():
    """Whether this process may install a seccomp filter, as a worker does:
    no_new_privs, then an allow-all filter, in a forked child."""
    pid = os.fork()
    if pid == 0:
        try:
            libc = ctypes.CDLL(None, use_errno=True)
            if libc.prctl(38, 1, 0, 0, 0) != 0:  # PR_SET_NO_NEW_PRIVS
                os._exit(2)

            class Insn(ctypes.Structure):
                _fields_ = [("code", ctypes.c_uint16), ("jt", ctypes.c_uint8), ("jf", ctypes.c_uint8), ("k", ctypes.c_uint32)]

            class Prog(ctypes.Structure):
                _fields_ = [("len", ctypes.c_uint16), ("filter", ctypes.POINTER(Insn))]

            allow = (Insn * 1)(Insn(0x06, 0, 0, 0x7FFF0000))  # BPF_RET|BPF_K, SECCOMP_RET_ALLOW
            prog = Prog(1, allow)
            os._exit(0 if libc.prctl(22, 2, ctypes.byref(prog), 0, 0) == 0 else 3)  # PR_SET_SECCOMP, FILTER
        except BaseException:
            os._exit(4)
    return os.waitpid(pid, 0)[1] == 0


def seccomp_findings(status, actions, installs):
    """`status`: /proc/self/status; `actions`: /proc/sys/kernel/seccomp/actions_avail."""
    facts, warnings, problems = [], [], []
    mode = next((line.split()[1] for line in status.splitlines() if line.startswith("Seccomp:")), None)
    if mode is None:
        problems.append("this kernel has no seccomp: build it with CONFIG_SECCOMP_FILTER (workers install a syscall filter)")
        return facts, warnings, problems
    facts.append(f"seccomp: mode {mode} here; filter actions {actions.strip() or 'unknown'}")
    if mode == "2":
        warnings.append("this process already runs under a seccomp filter (a container's profile?): it must allow "
                        "seccomp and landlock_create_ruleset, landlock_add_rule, landlock_restrict_self, as "
                        "deploy/systemd's SystemCallFilter does")
    missing = [a for a in ("errno", "allow") if actions and a not in actions.split()]
    if missing:
        problems.append(f"the kernel's seccomp lacks the {' and '.join(missing)} action(s) the worker's filter uses: upgrade it")
    if not installs:
        problems.append("installing a seccomp filter failed here: workers would fail to jail themselves. "
                        "Allow seccomp(2)/prctl(PR_SET_SECCOMP) in the container or unit")
    return facts, warnings, problems


def caps_findings(status, euid):
    """A root supervisor needs CAPS in its effective set (`CapEff` in /proc/self/status)."""
    if euid != 0:
        return [], [], []
    capeff = next((int(line.split()[1], 16) for line in status.splitlines() if line.startswith("CapEff:")), 0)
    missing = [name for name, bit in CAPS.items() if not capeff >> bit & 1]
    if missing:
        return [], [], [f"root without {', '.join(missing)}: the supervisor gives each app its own uid, chowns its "
                         "directories and kills its worker. Grant them (CapabilityBoundingSet= in the unit, "
                         "--cap-add in a container) or run nanovm without root (one uid for all apps)"]
    return [f"root with {', '.join(CAPS)}"], [], []


def address_space_findings(need, soft, overcommit):
    """`ulimit -v` against the pooling allocator's reservations; strict overcommit."""
    facts, warnings, problems = [f"address space: each worker reserves about {need >> 30} GiB up front"], [], []
    if soft != resource.RLIM_INFINITY and soft < need:
        problems.append(f"ulimit -v is {soft >> 20} MiB: workers fail with \"failed to create memory pool mapping\". "
                        "Remove the limit (LimitAS=infinity, ulimit -v unlimited) or lower max_live_objects or "
                        "memory_reservation_mb")
    if need > 100 << 40:
        problems.append("that is more than a 47-bit address space holds: lower max_live_objects or memory_reservation_mb")
    if overcommit == "2":
        warnings.append("vm.overcommit_memory = 2 (strict) is untested with nanovm's large reservations: "
                        "use 0 (the default) or load-test this setting first")
    return facts, warnings, problems


def mount_findings(fstype, opts):
    """What the data directory's filesystem and its mount options mean for commits."""
    opts = opts.split(",")
    warnings, problems = [], []
    if "ro" in opts:
        problems.append("data_dir is on a read-only mount: remount it rw")
    if any(o in opts for o in ("nobarrier", "barrier=0")):
        problems.append("data_dir is mounted with nobarrier: an fsync may not reach the disk, and commits rely on it. "
                        "Remove the option")
    if any(o in opts for o in ("sync", "dirsync")):
        warnings.append("data_dir is mounted sync/dirsync: every write waits for the disk although nanovm syncs at "
                        "commit; remove the option unless you need it")
    if fstype == "overlay":
        warnings.append("data_dir is on an overlay (a container's own filesystem): put it on a volume or bind mount")
    if fstype in ("nfs", "nfs4", "cifs", "smb3", "9p", "virtiofs") or fstype.startswith("fuse"):
        warnings.append(f"data_dir is on {fstype}: its flock and fsync semantics are untested with nanovm; "
                        "prefer a local ext4 or XFS")
    return warnings, problems


def fs_probe(directory, capture):
    """The calls the journal and capture make, in a scratch directory under
    `directory`: file and directory fsync, flock, a rename between
    directories, a hard link."""
    problems, warnings = [], []
    probe = Path(directory) / f".nanovm-doctor-{os.getpid()}"
    steps = []
    try:
        probe.mkdir()
        (probe / "a").mkdir()
        (probe / "b").mkdir()
        steps.append("file fsync")
        with open(probe / "a" / "f", "wb") as f:
            f.write(b"x")
            os.fsync(f.fileno())
        steps.append("directory fsync")
        fd = os.open(probe / "a", os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(fd)
            steps.append("flock")
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        finally:
            os.close(fd)
        steps.append("rename between directories")
        os.rename(probe / "a" / "f", probe / "b" / "f")
        steps.append("hard link")
        os.link(probe / "b" / "f", probe / "a" / "g")
        steps.append("")
    except OSError as e:
        failed = steps[-1] if steps else "creating a directory"
        msg = f"{failed} failed in {directory} ({e.strerror})"
        if failed == "hard link" and not capture:
            warnings.append(f"{msg}: local capture ([capture]) needs hard links; use another filesystem before enabling it")
        elif failed == "creating a directory":
            warnings.append(f"{msg}: run this as nanovm's user to probe the filesystem")
        else:
            problems.append(f"{msg}: the journal needs it. Put data_dir on a local ext4 or XFS")
    finally:
        shutil.rmtree(probe, ignore_errors=True)
    return warnings, problems


def cgroup_findings(mounts, g, wanted):
    """cgroup v2 at /sys/fs/cgroup, and the delegated directory `g` (if set)
    able to hold one child per app with `wanted` controllers."""
    facts, warnings, problems = [], [], []
    unified = any(line.split()[1:3] == ["/sys/fs/cgroup", "cgroup2"] for line in mounts.splitlines() if len(line.split()) > 2)
    facts.append("cgroup v2 at /sys/fs/cgroup" if unified else "cgroup v1 or hybrid at /sys/fs/cgroup")
    if not unified and (g or wanted):
        problems.append("[worker] limits need cgroup v2: boot with systemd.unified_cgroup_hierarchy=1 (most current "
                        "distributions default to it)")
    if not g:
        return facts, warnings, problems
    g = Path(g)
    if not g.is_dir():
        return facts, warnings, problems  # check() reports a missing directory
    available = (g / "cgroup.controllers").read_text().split() if (g / "cgroup.controllers").exists() else []
    lacking = [c for c in wanted if c not in available]
    if lacking:
        problems.append(f"{g.parent} does not hand {' '.join(lacking)} down to {g}: "
                        f"echo +{' +'.join(lacking)} > {g.parent}/cgroup.subtree_control (Delegate= in the unit)")
    procs = (g / "cgroup.procs").read_text().split() if (g / "cgroup.procs").exists() else []
    if procs and wanted:
        problems.append(f"{g} holds processes ({' '.join(procs[:5])}): a cgroup that hands controllers to its "
                        "children cannot. Move them to a leaf (deploy/systemd: DelegateSubgroup=supervisor)")
    if not os.access(g, os.W_OK) or not os.access(g / "cgroup.subtree_control", os.W_OK):
        problems.append(f"{g} is not writable by this user: the supervisor makes <cgroup>/<app> there. "
                        "Delegate it to nanovm's user (Delegate= in the unit, or chown)")
    return facts, warnings, problems


def adjtimex_state():
    """The kernel clock's state: 5 (TIME_ERROR) means unsynchronized."""
    try:
        buf = ctypes.create_string_buffer(512)  # struct timex, modes = 0: read only
        return ctypes.CDLL(None, use_errno=True).adjtimex(buf)
    except (OSError, AttributeError):
        return None


def clock_findings(source, state):
    facts, warnings = [f"clock source {source or 'unknown'}"], []
    if source in ("hpet", "acpi_pm", "jiffies", "refined-jiffies", "pit"):
        warnings.append(f"clock source {source} makes every clock read a system call; nanovm reads the clock for "
                        "each request's CPU budget and deadline. Use tsc or the hypervisor's clock (kvm-clock) if "
                        "the host offers one (clocksource= on the kernel command line)")
    if state == 5:
        warnings.append("the system clock is not synchronized (adjtimex reports TIME_ERROR): TLS to registries and "
                        "egress hosts, and guests' wall clock, need the right time. Run chrony or systemd-timesyncd")
    return facts, warnings


def read(path):
    try:
        return Path(path).read_text()
    except OSError:
        return ""


def check(cfg_path=None, nofile=None):
    raw = tomllib.loads(Path(cfg_path).read_text()) if cfg_path else {}
    cfg = {**DEFAULTS, **raw}
    cfg["limits"] = {**LIMIT_DEFAULTS, **raw.get("limits", {})}
    cfg["worker"] = raw.get("worker", {})
    # As NodeConfig::load resolves it; without a config, from here.
    data = (Path(cfg_path).resolve().parent if cfg_path else Path.cwd()) / cfg["data_dir"]
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

    def add(found):
        f, w, p = found
        facts.extend(f)
        warnings.extend(w)
        problems.extend(p)

    add(address_space_findings(need["address_space"], resource.getrlimit(resource.RLIMIT_AS)[0],
                               read("/proc/sys/vm/overcommit_memory").strip()))
    add(landlock_findings(landlock_abi()))
    add(seccomp_findings(read("/proc/self/status"), read("/proc/sys/kernel/seccomp/actions_avail"), seccomp_probe()))
    if os.geteuid() != 0:
        warnings.append("not root: if nanovm also runs without root, every app shares one uid")
    add(caps_findings(read("/proc/self/status"), os.geteuid()))

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
            warnings.append("btrfs held at 39-46 durable writes/s at 64 concurrent in every measured round (experiments/fs-matrix)")
        if "discard" in opts.split(",") and fstype in ("ext4", "xfs"):
            warnings.append("online discard lowered concurrent durable writes in 5 of 6 measured rounds (experiments/fs-matrix); prefer fstrim.timer")
        w, p = mount_findings(fstype, opts)
        warnings.extend(w)
        problems.extend(p)
        w, p = fs_probe(probe, raw.get("capture") is not None)
        warnings.extend(w)
        problems.extend(p)

    capture = raw.get("capture")
    history = data.is_dir() and any((data / "objects").glob("*/~committed/*"))
    if capture is not None:
        mb = {"max_mb": 256, "max_versions": 16, **capture}
        facts.append(f"local capture on: each object's history holds up to {mb['max_mb']} MiB "
                     f"and {mb['max_versions']} versions (full copies); a full one refuses writes")
    elif history:
        problems.append("objects have capture history (~committed) but [capture] is off: they will refuse to start")

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
    wanted = [c for c, k in (("memory", "memory_mb"), ("cpu", "cpus"), ("pids", "pids")) if cfg["worker"].get(k)]
    add(cgroup_findings(read("/proc/self/mounts"), cfg.get("cgroup"), wanted))
    f, w = clock_findings(read("/sys/devices/system/clocksource/clocksource0/current_clocksource").strip(), adjtimex_state())
    facts.extend(f)
    warnings.extend(w)

    if not any(Path(p).exists() for p in ("/etc/ssl/certs/ca-certificates.crt", "/etc/pki/tls/certs/ca-bundle.crt")):
        problems.append("no CA bundle found: the node refuses to start without CA certificates")
    if os.environ.get("RUST_BACKTRACE"):
        warnings.append("RUST_BACKTRACE is set in this environment: don't pass it to nanovm (warm throughput 63K/s -> 23K/s)")
    return facts, warnings, problems


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("config", nargs="?", help="the node's nanovm.toml (default: the defaults, data_dir under here)")
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
