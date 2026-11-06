"""CLI contract tests with a real disposable restic repository, no network."""

import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("backup.py").resolve()


class BackupTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.root = self.base / "source"
        self.app = self.root / "objects" / "tenant"
        (self.app / "object").mkdir(parents=True)
        (self.app / "object" / "data").write_bytes(b"committed\x00bytes")
        (self.app / "~journal" / "object").mkdir(parents=True)
        (self.app / "~journal" / "object" / "log").write_bytes(b"undo must survive")
        (self.root / "tmp").mkdir()
        (self.root / "tmp" / "secret").write_text("do not copy scratch")
        self.env = dict(os.environ, RESTIC_REPOSITORY=str(self.base / "repository"),
                        RESTIC_PASSWORD="disposable-test-only", RESTIC_CACHE_DIR=str(self.base / "cache"))
        subprocess.run(["restic", "init"], env=self.env, check=True, capture_output=True)

    def cli(self, *args, succeeds=True):
        result = subprocess.run([sys.executable, str(SCRIPT), *map(str, args)],
                                cwd=self.base, env=self.env, text=True, capture_output=True)
        self.assertEqual(result.returncode == 0, succeeds, result.stdout + result.stderr)
        return result.stdout.strip().splitlines()[-1] if result.stdout.strip() else ""

    def test_roundtrip_journals_empty_directories_and_existing_destination(self):
        (self.app / "object" / "empty").mkdir()
        snapshot = self.cli("backup", self.root, "--stopped")
        self.assertEqual(len(snapshot), 64)
        destination = self.base / "fresh"
        self.cli("restore", snapshot, destination)
        self.assertEqual((destination / "objects/tenant/object/data").read_bytes(), b"committed\x00bytes")
        self.assertEqual((destination / "objects/tenant/~journal/object/log").read_bytes(), b"undo must survive")
        self.assertTrue((destination / "objects/tenant/object/empty").is_dir())
        self.assertEqual([p.name for p in destination.iterdir()], ["objects"])
        self.cli("restore", snapshot, destination, succeeds=False)
        empty = self.base / "empty"
        empty.mkdir()
        self.cli("restore", snapshot, empty, succeeds=False)
        link = self.base / "link"
        link.symlink_to(self.base / "missing")
        self.cli("restore", snapshot, link, succeeds=False)
        self.assertFalse((self.base / "missing").exists())

    def test_owner_and_explicit_quiescence_are_required(self):
        self.cli("backup", self.root, succeeds=False)
        fd = os.open(self.app, os.O_RDONLY | os.O_DIRECTORY)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.cli("backup", self.root, "--stopped", succeeds=False)
        finally:
            os.close(fd)
        snapshots = subprocess.check_output(["restic", "snapshots", "--json"], env=self.env, text=True)
        self.assertEqual(json.loads(snapshots), [])
        self.cli("backup", self.root, "--stopped")

    def test_unapproved_snapshot_and_ambiguous_selectors_are_refused(self):
        subprocess.run(["restic", "backup", "objects"], cwd=self.root, env=self.env,
                       check=True, capture_output=True)
        snapshots = json.loads(subprocess.check_output(["restic", "snapshots", "--json"], env=self.env))
        for snapshot in ["latest", snapshots[0]["id"], snapshots[0]["short_id"]]:
            self.cli("restore", snapshot, self.base / "fresh", succeeds=False)
            self.assertFalse((self.base / "fresh").exists())

    def test_missing_repository_content_is_not_a_successful_restore(self):
        snapshot = self.cli("backup", self.root, "--stopped")
        for pack in (self.base / "repository/data").glob("*/*"):
            pack.unlink()
        self.cli("restore", snapshot, self.base / "fresh", succeeds=False)

    def test_incomplete_backup_never_gets_a_completion_tag(self):
        if os.geteuid() == 0:
            self.skipTest("requires an unprivileged reader to induce EACCES")
        unreadable = self.app / "object/data"
        unreadable.chmod(0)
        try:
            self.cli("backup", self.root, "--stopped", succeeds=False)
        finally:
            unreadable.chmod(0o600)
        snapshots = json.loads(subprocess.check_output(["restic", "snapshots", "--json"], env=self.env))
        self.assertEqual(len(snapshots), 1, "restic saves an incomplete snapshot on exit 3")
        self.assertFalse(snapshots[0].get("tags"))
        self.cli("restore", snapshots[0]["id"], self.base / "fresh", succeeds=False)

    def test_numeric_owners_and_modes_survive_root_restore(self):
        if os.geteuid() != 0:
            self.skipTest("run as root to verify jailed UID/GID restoration")
        for path in [self.app, self.app / "~journal/object", self.app / "object/data"]:
            os.chown(path, 200001, 200002)
            path.chmod(0o700 if path.is_dir() else 0o600)
        snapshot = self.cli("backup", self.root, "--stopped")
        self.cli("restore", snapshot, self.base / "fresh")
        for name, mode in [("", 0o700), ("~journal/object", 0o700), ("object/data", 0o600)]:
            md = (self.base / "fresh/objects/tenant" / name).stat()
            self.assertEqual((md.st_uid, md.st_gid, md.st_mode & 0o777), (200001, 200002, mode))

    def test_relative_repository_credentials_and_cache_keep_the_callers_cwd(self):
        (self.base / "password").write_text(self.env.pop("RESTIC_PASSWORD"))
        self.env.update(RESTIC_REPOSITORY="repository", RESTIC_PASSWORD_FILE="password", RESTIC_CACHE_DIR="cache")
        snapshot = self.cli("backup", "source", "--stopped")
        self.cli("restore", snapshot, "fresh")
        self.assertEqual((self.base / "fresh/objects/tenant/object/data").read_bytes(), b"committed\x00bytes")

    def test_final_sync_failure_does_not_report_success_or_allow_retry_in_place(self):
        snapshot = self.cli("backup", self.root, "--stopped")
        commands = self.base / "commands"
        commands.mkdir()
        (commands / "sync").write_text("#!/bin/sh\nexit 1\n")
        (commands / "sync").chmod(0o755)
        self.env["PATH"] = str(commands) + os.pathsep + os.environ["PATH"]
        destination = self.base / "fresh"
        self.cli("restore", snapshot, destination, succeeds=False)
        self.assertEqual((destination / "objects/tenant/object/data").read_bytes(), b"committed\x00bytes")
        self.env["PATH"] = os.environ["PATH"]
        self.cli("restore", snapshot, destination, succeeds=False)

    def test_symlink_app_and_recursive_repository_are_refused(self):
        (self.root / "objects/alias").symlink_to(self.app)
        self.cli("backup", self.root, "--stopped", succeeds=False)
        (self.root / "objects/alias").unlink()
        self.env["RESTIC_REPOSITORY"] = "local:" + str(self.app / "repository")
        self.cli("backup", self.root, "--stopped", succeeds=False)
        self.assertFalse((self.app / "repository").exists())


if __name__ == "__main__":
    unittest.main()
