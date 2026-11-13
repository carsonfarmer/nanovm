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

    def run_check(self, toml, nofile=1 << 20, address_space="infinity"):
        path = self.root / "nanovm.toml"
        path.write_text(toml)
        return check_host.check(path, nofile, address_space)

    def test_needs_match_engine_preflight(self):
        # engine::preflight: n = max_live_objects + 32; fds 2n + 1024; maps 24n + 4096.
        self.assertEqual(check_host.needs({"max_live_objects": 8000, "limits": {"disk_mb": 64}})["fds"], 17088)
        self.assertEqual(check_host.needs({"max_live_objects": 8000, "limits": {"disk_mb": 64}})["maps"], 196864)
        n = check_host.needs({"max_live_objects": 1000, "limits": {"disk_mb": 8}})
        self.assertEqual((n["fds"], n["maps"], n["disk_per_busy_object_mb"]), (3088, 28864, 24))

    def test_address_space_matches_engine_preflight(self):
        # engine::preflight: n x (4 x (memory_reservation_mb MiB + 64 KiB) + 4 MiB) + 1 GiB.
        n = check_host.needs({"max_live_objects": 1000, "limits": {"disk_mb": 64}})
        self.assertEqual(n["address_space"], 1032 * (4 * ((128 << 20) + (64 << 10)) + (4 << 20)) + (1 << 30))
        _, _, problems = self.run_check('data_dir = "data"\n', address_space=64 << 30)
        self.assertTrue(any("address-space limit" in p for p in problems), problems)
        facts, _, problems = self.run_check('data_dir = "data"\nmax_live_objects = 10\n', address_space=64 << 30)
        self.assertFalse(any("address-space" in p for p in problems), problems)
        self.assertTrue(any("reserves about 23 GiB" in f for f in facts), facts)

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
        self.assertEqual([p for p in problems if "cgroup" in p], [f"{g}/cgroup.subtree_control lacks memory, needed by [worker] limits"])

    def test_capture_history_needs_capture(self):
        hist = self.root / "data/objects/app/~committed/o"
        hist.mkdir(parents=True)
        (hist / "00000000000000000001.tar").write_bytes(b"")
        _, _, problems = self.run_check('data_dir = "data"\n')
        self.assertTrue(any("refuse to start" in p for p in problems), problems)
        facts, _, problems = self.run_check('data_dir = "data"\n[capture]\nmax_mb = 64\n')
        self.assertFalse(any("refuse to start" in p for p in problems), problems)
        self.assertTrue(any("up to 64 MiB and 16 versions" in f for f in facts), facts)


if __name__ == "__main__":
    unittest.main()
