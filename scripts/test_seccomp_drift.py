"""Parser tests for seccomp-drift.py on a synthetic `strace -f` trace."""

import importlib.util
from pathlib import Path
import unittest

SCRIPT = Path(__file__).with_name("seccomp-drift.py")
spec = importlib.util.spec_from_file_location("seccomp_drift", SCRIPT)
drift = importlib.util.module_from_spec(spec)
spec.loader.exec_module(drift)

# 100 is the test harness. It spawns 200 (a worker), which jails itself and
# starts thread 201; 202 was started before the jail and keeps running.
# 300 is a process that is not a worker.
TRACE = """\
100 openat(AT_FDCWD, "/etc/hosts", O_RDONLY) = 3
100 socket(AF_INET, SOCK_STREAM, 0) = 4
100 clone3({flags=CLONE_VM|CLONE_VFORK|CLONE_CLEAR_SIGHAND, exit_signal=SIGCHLD, stack=0x7f, stack_size=0x9000}, 88 <unfinished ...>
200 execve("/target/debug/nanovm", ["/target/debug/nanovm", "worker"], 0x7f /* 0 vars */) = 0
100 <... clone3 resumed> => {parent_tid=[0]}, 88) = 200
200 read(0, ""..., 8192) = 512
200 clone(child_stack=0x7f, flags=CLONE_VM|CLONE_FS|CLONE_FILES|CLONE_SIGHAND|CLONE_THREAD|CLONE_SYSVSEM, parent_tid=[202]) = 202
200 socket(AF_INET, SOCK_STREAM, 0) = 5
200 seccomp(SECCOMP_SET_MODE_FILTER, 0, {len=470, filter=0x7f}) = 0
200 clone3({flags=CLONE_VM|CLONE_FS|CLONE_FILES|CLONE_SIGHAND|CLONE_THREAD|CLONE_SYSVSEM, child_tid=0x7f}, 88) = -1 ENOSYS (Function not implemented)
200 clone(child_stack=0x7f, flags=CLONE_VM|CLONE_FS|CLONE_FILES|CLONE_SIGHAND|CLONE_THREAD|CLONE_SYSVSEM, parent_tid=[201] <unfinished ...>
201 futex(0x7f, FUTEX_WAIT_PRIVATE, 0, NULL <unfinished ...>
200 <... clone resumed>) = 201
201 <... futex resumed>) = 0
201 openat(AT_FDCWD, "/data/x", O_RDONLY) = 6
201 fcntl(6, F_SETLK, {l_type=F_WRLCK}) = -1 ENOSYS (Function not implemented)
201 socket(AF_INET, SOCK_STREAM, 0) = -1 ENOSYS (Function not implemented)
201 sched_yield() = 0
202 socket(AF_INET, SOCK_STREAM, 0) = 7
100 vfork() = 300
300 execve("/bin/true", ["true"], 0x7f /* 0 vars */) = 0
300 socket(AF_INET, SOCK_STREAM, 0) = -1 ENOSYS (Function not implemented)
"""

ALLOWED = {"openat", "futex", "fcntl", "clone", "read", "sched_yield", "close"}
BY_ARGS = {"fcntl", "clone"}


class DriftTest(unittest.TestCase):
    def setUp(self):
        self.result = drift.analyse(TRACE.splitlines(), ALLOWED, BY_ARGS)

    def test_only_jailed_worker_threads_count(self):
        r = self.result
        self.assertEqual((r["workers"], r["jailed_workers"]), (1, 1))
        # The harness, the worker before its jail, and process 300 are not counted.
        self.assertEqual(r["observed"], {"clone": 1, "clone3": 1, "fcntl": 1, "futex": 1, "openat": 1,
                                         "sched_yield": 1, "socket": 1})

    def test_denials_are_drift_unless_expected(self):
        d = self.result["drift"]
        self.assertEqual(set(d), {"fcntl", "socket"})
        self.assertTrue(d["fcntl"]["allowlisted_with_argument_rules"])
        self.assertIn("F_SETLK", d["fcntl"]["examples"][0])
        self.assertFalse(d["socket"]["allowlisted_with_argument_rules"])
        self.assertEqual(self.result["expected_denials"], {"clone3": 1})

    def test_a_thread_started_before_the_jail_is_reported(self):
        self.assertEqual(self.result["unjailed_after_jail"], {"socket": 1})

    def test_unused_entries_and_resumed_calls(self):
        self.assertEqual(self.result["unused_allowlist"], ["close", "read"])
        self.assertEqual(self.result["inconsistent"], {})
        # A refused call that succeeds can only be a parser or filter bug.
        bad = TRACE + "201 socket(AF_UNIX, SOCK_DGRAM, 0) = 8\n"
        self.assertEqual(list(drift.analyse(bad.splitlines(), ALLOWED, BY_ARGS)["inconsistent"]), ["socket"])

    def test_allowlist_parses_jail_rs(self):
        source = drift.JAIL.read_text()
        allowed, by_args = drift.allowlist(source, "x86_64")
        self.assertTrue({"futex", "openat", "mmap", "clone", "fcntl", "unlink", "epoll_wait"} <= allowed)
        self.assertTrue({"mmap", "mprotect", "clone", "fcntl", "socketpair", "ioctl"} <= by_args)
        self.assertNotIn("socket", allowed)
        self.assertNotIn("clone3", allowed)
        arm, _ = drift.allowlist(source, "aarch64")
        self.assertNotIn("unlink", arm)
        self.assertIn("unlinkat", arm)


if __name__ == "__main__":
    unittest.main()
