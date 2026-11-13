"""Tests for changelog.py against a throwaway repository.

    python3 -m unittest discover -s tools/release -v
"""

import os
import subprocess
import tempfile
import unittest
from unittest import mock

import changelog


class Changelog(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.repo = self.dir.name
        env = {"GIT_AUTHOR_DATE": "2026-11-01T00:00:00Z", "GIT_COMMITTER_DATE": "2026-11-01T00:00:00Z"}
        self.git = lambda *a: subprocess.run(["git", "-C", self.repo, *a], check=True, capture_output=True,
                                             text=True, env={**os.environ, **env}).stdout
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "t@example.com")
        self.git("config", "user.name", "t")
        self.commit("First")
        self.git("tag", "v0.1.0")
        self.commit("Jail: allow only stream socketpairs (N-70)")
        self.git("checkout", "-q", "-b", "side")
        self.commit("Side work")
        self.git("checkout", "-q", "main")
        self.commit("Proxy: refuse forged events")
        self.git("merge", "-q", "--no-ff", "-m", "Merge branch 'side'", "side")
        self.git("tag", "v0.2.0")
        self.commit("After the release")

    def tearDown(self):
        self.dir.cleanup()

    def commit(self, subject):
        self.git("commit", "-q", "--allow-empty", "-m", subject, "-m", "body text is not listed")

    def test_lists_commits_since_the_previous_tag_without_merges(self):
        with mock.patch.dict(os.environ, {"GITHUB_REPOSITORY": "o/r"}):
            text = changelog.notes("v0.2.0", repo=self.repo)
        lines = text.splitlines()
        self.assertEqual(lines[0], "## nanovm v0.2.0 (2026-11-01)")
        self.assertIn("3 commits since v0.1.0.", text)
        subjects = [line.rsplit(" (", 1)[0] for line in lines if line.startswith("- ")]
        self.assertEqual(sorted(subjects), ["- Jail: allow only stream socketpairs (N-70)",
                                            "- Proxy: refuse forged events", "- Side work"])
        self.assertNotIn("Merge", text)
        self.assertNotIn("After the release", text)
        self.assertNotIn("body text", text)
        self.assertTrue(text.endswith("Full diff: https://github.com/o/r/compare/v0.1.0...v0.2.0\n"))

    def test_first_release_lists_everything(self):
        text = changelog.notes("v0.1.0", repo=self.repo)
        self.assertIn("1 commit since the first commit.", text)
        self.assertIn("- First (", text)
        self.assertNotIn("Full diff", text)

    def test_explicit_since_and_untagged_preview(self):
        text = changelog.notes("HEAD", since="v0.2.0", repo=self.repo)
        self.assertRegex(text.splitlines()[0], r"^## nanovm [0-9a-f]{7,} \(2026-11-01\)$")
        self.assertIn("1 commit since v0.2.0.", text)
        self.assertIn("- After the release (", text)

    def test_previous_tag_ignores_other_tags(self):
        self.git("tag", "not-a-version", "v0.2.0")
        self.assertEqual(changelog.previous_tag("HEAD", self.repo), "v0.2.0")
        self.assertIsNone(changelog.previous_tag("v0.1.0", self.repo))


if __name__ == "__main__":
    unittest.main()
