"""Tests for volume.py.  python3 -m unittest discover -s tools -p test_volume.py -v

The round trip through a real node (SQL, KV and files) is in
crates/nanovm/tests/volume.rs.
"""

import fcntl
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("volume.py").resolve()
APP = "example.com_registry_org_app-0123456789abcdef"


def capture_tar(path, app, obj, sequence, parent, files):
    """A TAR in local capture's layout (committed.rs::prepare)."""
    with tarfile.open(path, "w", format=tarfile.GNU_FORMAT) as tar:
        meta = json.dumps({"format": 1, "app": app, "object": obj, "sequence": sequence, "parent": parent}).encode()
        info = tarfile.TarInfo("nanovm.json")
        info.size = len(meta)
        tar.addfile(info, io.BytesIO(meta))
        d = tarfile.TarInfo("data")
        d.type = tarfile.DIRTYPE
        tar.addfile(d)
        for name, data in files.items():
            info = tarfile.TarInfo(f"data/{name}")
            info.size = len(data)
            tar.addfile(info, io.BytesIO(data))


class VolumeTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.root = self.base / "root"
        self.app = self.root / "objects" / APP
        vol = self.app / "alice"
        (vol / ".kv" / "default").mkdir(parents=True)
        (vol / ".kv" / "default" / "bWFya2Vy").write_bytes(b"kv value")
        (vol / "empty").mkdir()
        (vol / "deep" / "er").mkdir(parents=True)
        (vol / "deep" / "er" / "blob").write_bytes(bytes(range(256)) * 300)
        (vol / "count").write_text("7")
        os.chmod(vol / "count", 0o600)

    def cli(self, *args, succeeds=True):
        r = subprocess.run([sys.executable, str(SCRIPT), *map(str, args)], text=True, capture_output=True)
        self.assertEqual(r.returncode == 0, succeeds, r.stdout + r.stderr)
        return r.stdout + r.stderr

    def tree(self, vol):
        out = {}
        for dirpath, dirnames, filenames in os.walk(vol):
            for n in dirnames + filenames:
                p = Path(dirpath) / n
                rel = str(p.relative_to(vol))
                out[rel] = (oct(p.stat().st_mode), None if p.is_dir() else p.read_bytes())
        return out

    def test_round_trip_into_a_fresh_root(self):
        out = self.base / "alice.tar"
        self.cli("export", self.root, APP, "alice", out)
        with tarfile.open(out) as tar:
            names = tar.getnames()
            meta = json.loads(tar.extractfile(names[0]).read())
        self.assertEqual(names[:2], ["nanovm.json", "data"])
        self.assertEqual(meta, {"format": 1, "app": APP, "object": "alice", "sequence": 0, "parent": ""})
        fresh = self.base / "fresh"
        self.cli("import", out, fresh)
        self.assertEqual(self.tree(fresh / "objects" / APP / "alice"), self.tree(self.app / "alice"))
        self.assertEqual(sorted(p.name for p in (fresh / "objects" / APP).iterdir()), ["alice"], "no staging left")
        # Under another name, in another app.
        self.cli("import", out, fresh, "--app", "other", "--object", "bob")
        self.assertEqual(self.tree(fresh / "objects/other/bob"), self.tree(self.app / "alice"))

    def test_export_needs_the_app_lock_and_no_pending_undo(self):
        fd = os.open(self.app, os.O_RDONLY | os.O_DIRECTORY)
        fcntl.flock(fd, fcntl.LOCK_EX)
        try:
            self.assertIn("worker is running", self.cli("export", self.root, APP, "alice", self.base / "x.tar", succeeds=False))
        finally:
            os.close(fd)
        (self.app / "~journal" / "alice").mkdir(parents=True)
        (self.app / "~journal" / "alice" / "log").write_bytes(b"undo")
        self.assertIn("uncommitted", self.cli("export", self.root, APP, "alice", self.base / "x.tar", succeeds=False))
        self.assertFalse((self.base / "x.tar").exists())
        self.cli("export", self.root, APP, "../alice", self.base / "x.tar", succeeds=False)
        (self.app / "alice" / "link").symlink_to("count")
        (self.app / "~journal" / "alice" / "log").unlink()
        self.assertIn("neither a file", self.cli("export", self.root, APP, "alice", self.base / "x.tar", succeeds=False))

    def test_import_only_into_a_fresh_place(self):
        out = self.base / "alice.tar"
        self.cli("export", self.root, APP, "alice", out)
        self.assertIn("exists", self.cli("import", out, self.root, succeeds=False))
        fresh = self.base / "fresh"
        for leftover in ("~journal/alice", "~committed/alice"):
            (fresh / "objects" / APP / leftover).mkdir(parents=True)
            self.assertIn("exists", self.cli("import", out, fresh, succeeds=False))
            (fresh / "objects" / APP / leftover).rmdir()
        self.assertFalse((fresh / "objects" / APP / "alice").exists())
        fd = os.open(fresh / "objects" / APP, os.O_RDONLY | os.O_DIRECTORY)
        fcntl.flock(fd, fcntl.LOCK_EX)
        try:
            self.assertIn("locked", self.cli("import", out, fresh, succeeds=False))
        finally:
            os.close(fd)

    def test_import_refuses_unsafe_archives(self):
        def archive(*members, identity=True):
            path = self.base / f"bad{len(list(self.base.glob('bad*')))}.tar"
            with tarfile.open(path, "w") as tar:
                if identity:
                    meta = json.dumps({"format": 1, "app": APP, "object": "o", "sequence": 0, "parent": ""}).encode()
                    info = tarfile.TarInfo("nanovm.json")
                    info.size = len(meta)
                    tar.addfile(info, io.BytesIO(meta))
                d = tarfile.TarInfo("data")
                d.type = tarfile.DIRTYPE
                tar.addfile(d)
                for name, kind in members:
                    info = tarfile.TarInfo(name)
                    info.type = kind
                    if kind in (tarfile.SYMTYPE, tarfile.LNKTYPE):
                        info.linkname = "/etc/passwd"
                    tar.addfile(info, io.BytesIO(b""))
            return path
        fresh = self.base / "fresh"
        for bad in [archive(("data/../escape", tarfile.REGTYPE)), archive(("/abs", tarfile.REGTYPE)),
                    archive(("data/s", tarfile.SYMTYPE)), archive(("data/h", tarfile.LNKTYPE)),
                    archive(("outside", tarfile.REGTYPE)), archive(("data/f", tarfile.FIFOTYPE)),
                    archive(("data/x", tarfile.REGTYPE), ("data/x", tarfile.REGTYPE)),
                    archive(identity=False)]:
            self.cli("import", bad, fresh, succeeds=False)
            self.assertFalse((fresh / "objects" / APP / "o").exists(), bad)
            self.assertEqual([p.name for p in (fresh / "objects" / APP).iterdir()] if (fresh / "objects" / APP).exists() else [], [])
        self.assertFalse((self.base / "escape").exists())

    def test_a_committed_version_is_an_export(self):
        history = self.app / "~committed" / "alice"
        history.mkdir(parents=True)
        capture_tar(history / f"{1:020}.tar", APP, "alice", 1, "", {"count": b"1"})
        parent = hashlib.sha256((history / f"{1:020}.tar").read_bytes()).hexdigest()
        capture_tar(history / f"{2:020}.tar", APP, "alice", 2, parent, {"count": b"2"})
        # No lock needed: versions are immutable once published.
        fd = os.open(self.app, os.O_RDONLY | os.O_DIRECTORY)
        fcntl.flock(fd, fcntl.LOCK_EX)
        try:
            out = self.base / "v.tar"
            self.cli("export", self.root, APP, "alice", out, "--committed")
            self.assertEqual(out.read_bytes(), (history / f"{2:020}.tar").read_bytes())
            self.cli("export", self.root, APP, "alice", self.base / "v1.tar", "--committed", "1")
            self.assertEqual((self.base / "v1.tar").read_bytes(), (history / f"{1:020}.tar").read_bytes())
        finally:
            os.close(fd)
        fresh = self.base / "fresh"
        self.cli("import", out, fresh)
        self.assertEqual((fresh / "objects" / APP / "alice" / "count").read_bytes(), b"2")
        # A broken chain, a gap or a foreign identity is refused.
        capture_tar(history / f"{3:020}.tar", APP, "alice", 3, "0" * 64, {})
        self.assertIn("parent", self.cli("export", self.root, APP, "alice", out, "--committed", succeeds=False))
        (history / f"{3:020}.tar").unlink()
        capture_tar(history / f"{4:020}.tar", APP, "alice", 4, "", {})
        self.assertIn("contiguous", self.cli("export", self.root, APP, "alice", out, "--committed", succeeds=False))
        (history / f"{4:020}.tar").unlink()
        capture_tar(history / f"{3:020}.tar", APP, "bob", 3, hashlib.sha256((history / f"{2:020}.tar").read_bytes()).hexdigest(), {})
        self.assertIn("names", self.cli("export", self.root, APP, "alice", out, "--committed", succeeds=False))

    @unittest.skipUnless(os.geteuid() == 0, "needs root")
    def test_root_import_gives_files_the_app_uid(self):
        out = self.base / "alice.tar"
        self.cli("export", self.root, APP, "alice", out)
        fresh = self.base / "fresh"
        (fresh / "objects" / APP).mkdir(parents=True)
        os.chown(fresh / "objects" / APP, 200_007, 200_007)
        self.cli("import", out, fresh)
        for dirpath, dirnames, filenames in os.walk(fresh / "objects" / APP):
            for n in [dirpath, *(os.path.join(dirpath, x) for x in dirnames + filenames)]:
                self.assertEqual((os.lstat(n).st_uid, os.lstat(n).st_gid), (200_007, 200_007), n)

    def test_list(self):
        (self.app / "~journal" / "alice").mkdir(parents=True)
        (self.app / "~journal" / "alice" / "log").write_bytes(b"")
        out = self.cli("list", self.root)
        self.assertIn(APP, out)
        self.assertIn("alice: 8 entries, 76809 bytes", out)
        self.assertIn("undo log pending", out)


if __name__ == "__main__":
    unittest.main()
