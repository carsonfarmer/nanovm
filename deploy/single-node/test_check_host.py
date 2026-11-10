"""Tests for check-host.py.  python3 -m unittest discover -s deploy/single-node -v"""

import importlib.util
import os
import tempfile
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location("check_host", Path(__file__).with_name("check-host.py"))
check_host = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check_host)


class CheckHost(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.root = Path(self.dir.name)
        os.chmod(self.root, 0o755)

    def tearDown(self):
        self.dir.cleanup()

    def run_check(self, toml, nofile=1 << 20):
        path = self.root / "nanovm.toml"
        path.write_text(toml)
        return check_host.check(path, nofile)

    def test_needs_match_engine_preflight(self):
        # engine::preflight: n = max_live_objects + 32; fds 2n + 1024; maps 24n + 4096.
        self.assertEqual(check_host.needs({"max_live_objects": 8000, "limits": {"disk_mb": 64}})["fds"], 17088)
        self.assertEqual(check_host.needs({"max_live_objects": 8000, "limits": {"disk_mb": 64}})["maps"], 196864)
        n = check_host.needs({"max_live_objects": 1000, "limits": {"disk_mb": 8}})
        self.assertEqual((n["fds"], n["maps"], n["disk_per_busy_object_mb"]), (3088, 28864, 24))

    def test_fd_limit_too_low_fails(self):
        _, _, problems = self.run_check('data_dir = "data"\n', nofile=1024)
        self.assertTrue(any("fd limit to at least 3088" in p for p in problems), problems)

    def test_unsearchable_parent_fails(self):
        locked = self.root / "locked"
        locked.mkdir(mode=0o700)
        _, _, problems = self.run_check(f'data_dir = "{locked}/data"\n')
        self.assertTrue(any(f"{locked} is not searchable" in p for p in problems), problems)
        os.chmod(locked, 0o755)
        _, _, problems = self.run_check(f'data_dir = "{locked}/data"\n')
        self.assertFalse(any("searchable" in p for p in problems), problems)

    def test_worker_limits_without_cgroup_warn_and_missing_cgroup_fails(self):
        _, warnings, _ = self.run_check('data_dir = "d"\n[worker]\nmemory_mb = 64\n')
        self.assertTrue(any("not applied" in w for w in warnings), warnings)
        _, _, problems = self.run_check(f'data_dir = "d"\ncgroup = "{self.root}/nope/apps"\n[worker]\npids = 10\n')
        self.assertTrue(any("doesn't exist" in p for p in problems), problems)

    def test_cgroup_missing_controller_fails(self):
        g = self.root / "apps"
        g.mkdir()
        (g / "cgroup.subtree_control").write_text("cpu pids\n")
        _, _, problems = self.run_check(f'data_dir = "d"\ncgroup = "{g}"\n[worker]\nmemory_mb = 64\npids = 10\n')
        # (Other cgroup findings depend on this host's hierarchy: test_cgroup_delegation.)
        self.assertIn(f"{g}/cgroup.subtree_control lacks memory, needed by [worker] limits", problems)

    def test_capture_history_needs_capture(self):
        hist = self.root / "data/objects/app/~committed/o"
        hist.mkdir(parents=True)
        (hist / "00000000000000000001.tar").write_bytes(b"")
        _, _, problems = self.run_check('data_dir = "data"\n')
        self.assertTrue(any("refuse to start" in p for p in problems), problems)
        facts, _, problems = self.run_check('data_dir = "data"\n[capture]\nmax_mb = 64\n')
        self.assertFalse(any("refuse to start" in p for p in problems), problems)
        self.assertTrue(any("up to 64 MiB and 16 versions" in f for f in facts), facts)

    def test_without_a_config_the_defaults_are_checked(self):
        cwd = os.getcwd()
        os.chdir(self.root)
        try:
            facts, _, problems = check_host.check(None, 1 << 20)
        finally:
            os.chdir(cwd)
        self.assertTrue(any("max_live_objects = 1000" in f for f in facts), facts)
        self.assertFalse(any("fd limit" in p for p in problems), problems)

    def test_landlock_needs_abi_2(self):
        self.assertTrue(check_host.landlock_findings(0)[2])
        self.assertIn("rename between directories", check_host.landlock_findings(1)[2][0])
        _, warnings, problems = check_host.landlock_findings(2)
        self.assertEqual((len(warnings), problems), (1, []))
        self.assertEqual(check_host.landlock_findings(6)[1:], ([], []))

    def test_seccomp(self):
        actions = "kill_process errno allow"
        self.assertEqual(check_host.seccomp_findings("Seccomp:\t0\n", actions, True)[1:], ([], []))
        self.assertIn("no seccomp", check_host.seccomp_findings("Name: x\n", actions, True)[2][0])
        self.assertIn("already runs under", check_host.seccomp_findings("Seccomp:\t2\n", actions, True)[1][0])
        self.assertIn("errno", check_host.seccomp_findings("Seccomp:\t0\n", "kill_process allow", True)[2][0])
        self.assertIn("failed here", check_host.seccomp_findings("Seccomp:\t0\n", actions, False)[2][0])
        # This host can install one (a forked child does).
        self.assertTrue(check_host.seccomp_probe())

    def test_root_needs_its_capabilities(self):
        everything = "CapEff:\t000001ffffffffff\n"
        self.assertEqual(check_host.caps_findings(everything, 0)[2], [])
        no_kill = "CapEff:\t%x\n" % (0x1FFFFFFFFFF & ~(1 << 5))
        self.assertIn("CAP_KILL", check_host.caps_findings(no_kill, 0)[2][0])
        self.assertEqual(check_host.caps_findings("CapEff:\t0\n", 1000), ([], [], []))

    def test_address_space(self):
        need = check_host.needs({"max_live_objects": 1, "limits": {"disk_mb": 64}})["address_space"]
        # 33 slots x 4 memories x (128 MiB + 64 KiB), plus 1 GiB: measured to
        # fail under 16 GiB and start under 17 GiB.
        self.assertEqual(need >> 20, 33 * 4 * 128 + 33 * 4 // 16 + 1024)
        self.assertEqual(check_host.address_space_findings(need, check_host.resource.RLIM_INFINITY, "0")[1:], ([], []))
        self.assertIn("ulimit -v", check_host.address_space_findings(need, 16 << 30, "0")[2][0])
        self.assertIn("overcommit", check_host.address_space_findings(need, check_host.resource.RLIM_INFINITY, "2")[1][0])

    def test_mounts(self):
        self.assertEqual(check_host.mount_findings("ext4", "rw,relatime"), ([], []))
        self.assertIn("read-only", check_host.mount_findings("ext4", "ro,relatime")[1][0])
        self.assertIn("nobarrier", check_host.mount_findings("ext4", "rw,nobarrier")[1][0])
        self.assertIn("sync", check_host.mount_findings("xfs", "rw,sync")[0][0])
        self.assertIn("overlay", check_host.mount_findings("overlay", "rw")[0][0])
        self.assertIn("nfs4", check_host.mount_findings("nfs4", "rw")[0][0])

    def test_filesystem_probe(self):
        self.assertEqual(check_host.fs_probe(self.root, True), ([], []))
        self.assertEqual(list(self.root.iterdir()), [], "the probe cleans up")
        warnings, problems = check_host.fs_probe(self.root / "missing", False)
        self.assertEqual(problems, [])
        self.assertIn("run this as nanovm's user", warnings[0])

    def test_cgroup_delegation(self):
        v2 = "cgroup2 /sys/fs/cgroup cgroup2 rw 0 0\n"
        v1 = "cgroup /sys/fs/cgroup/memory cgroup rw,memory 0 0\n"
        self.assertEqual(check_host.cgroup_findings(v1, None, [])[2], [])
        self.assertIn("cgroup v2", check_host.cgroup_findings(v1, None, ["memory"])[2][0])
        g = self.root / "nanovm.service" / "apps"
        g.mkdir(parents=True)
        (g / "cgroup.controllers").write_text("cpu pids\n")
        (g / "cgroup.subtree_control").write_text("")
        (g / "cgroup.procs").write_text("4242\n")
        _, _, problems = check_host.cgroup_findings(v2, str(g), ["memory", "pids"])
        self.assertIn("does not hand memory down", problems[0])
        self.assertIn("holds processes (4242)", problems[1])
        (g / "cgroup.controllers").write_text("cpu memory pids\n")
        (g / "cgroup.procs").write_text("")
        self.assertEqual(check_host.cgroup_findings(v2, str(g), ["memory", "pids"])[2], [])

    def test_clock(self):
        self.assertEqual(check_host.clock_findings("tsc", 0)[1], [])
        self.assertIn("system call", check_host.clock_findings("hpet", 0)[1][0])
        self.assertIn("not synchronized", check_host.clock_findings("kvm-clock", 5)[1][0])


if __name__ == "__main__":
    unittest.main()
