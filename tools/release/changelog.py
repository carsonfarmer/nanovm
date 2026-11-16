#!/usr/bin/env python3
"""Release notes from git history: the commits since the previous version tag.

    tools/release/changelog.py v0.2.0            # v0.2.0 against the tag before it
    tools/release/changelog.py v0.2.0 --since v0.1.0
    tools/release/changelog.py HEAD --since cf/main-next~20   # a preview

Prints Markdown: one line per commit (subject, short hash), oldest first, with
ISSUES ids (N-12, W-9) kept as written, and a compare link when
GITHUB_REPOSITORY is set (as in Actions). Merge commits are left out; their
branches' commits are listed. Review before publishing: subjects are copied
as committed.
"""

import argparse
import os
import re
import subprocess
import sys

VERSION_TAG = re.compile(r"v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?")


def git(*args, repo="."):
    return subprocess.run(["git", "-C", repo, *args], check=True, capture_output=True, text=True).stdout


def previous_tag(rev, repo="."):
    """The newest version tag reachable from rev's parents, or None."""
    try:
        out = git("describe", "--tags", "--abbrev=0", "--match", "v[0-9]*", f"{rev}^", repo=repo).strip()
    except subprocess.CalledProcessError:
        return None  # no earlier tag, or rev is the root commit
    return out if VERSION_TAG.fullmatch(out) else None


def commits(since, rev, repo="."):
    """(short hash, subject) for each non-merge commit in since..rev, oldest first."""
    span = f"{since}..{rev}" if since else rev
    out = git("log", "--no-merges", "--reverse", "--format=%h%x00%s%x00", span, repo=repo)
    fields = out.split("\0")
    return [(h.strip(), s) for h, s in zip(fields[0::2], fields[1::2]) if h.strip()]


def notes(rev, since=None, repo=".", date=None):
    since = since or previous_tag(rev, repo)
    entries = commits(since, rev, repo)
    title = rev if VERSION_TAG.fullmatch(rev) else git("rev-parse", "--short", rev, repo=repo).strip()
    date = date or git("log", "-1", "--format=%cs", rev, repo=repo).strip()
    lines = [f"## nanovm {title} ({date})", ""]
    base = f"since {since}" if since else "since the first commit"
    lines.append(f"{len(entries)} commit{'s' if len(entries) != 1 else ''} {base}.")
    lines.append("")
    lines += [f"- {subject} ({h})" for h, subject in entries]
    server, repository = os.environ.get("GITHUB_SERVER_URL", "https://github.com"), os.environ.get("GITHUB_REPOSITORY")
    if repository and since:
        lines += ["", f"Full diff: {server}/{repository}/compare/{since}...{rev}"]
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("rev", help="the release tag (or any revision, for a preview)")
    parser.add_argument("--since", help="start after this revision (default: the previous version tag)")
    parser.add_argument("--repo", default=".")
    args = parser.parse_args()
    try:
        sys.stdout.write(notes(args.rev, args.since, args.repo))
    except subprocess.CalledProcessError as e:
        sys.exit(f"changelog: git {' '.join(e.cmd[3:])} failed: {e.stderr.strip()}")


if __name__ == "__main__":
    main()
