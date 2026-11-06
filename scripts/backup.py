#!/usr/bin/env python3
"""Offline nanovm snapshots using restic; see README's Backup and restore section."""

import argparse
import contextlib
import fcntl
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import uuid

TAG = "nanovm-offline-v1:"


def restic(*args, **kwargs):
    return subprocess.check_output(["restic", *args], text=True, **kwargs)


def backup(root):
    root = root.resolve(strict=True)
    repository = os.environ["RESTIC_REPOSITORY"].removeprefix("local:")
    if ":" not in repository and Path(repository).resolve().is_relative_to(root):
        raise ValueError("the backup repository must be outside the data root")
    with contextlib.ExitStack() as locks:
        objects = root / "objects"
        if objects.is_symlink():
            raise ValueError("objects must be a real directory")
        held = []
        # Workers and supervisor preparation use flock on these same inodes.
        # This catches lingering owners, not a supervisor admitting new apps.
        for app in sorted(objects.iterdir()):
            fd = os.open(app, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
            locks.callback(os.close, fd)
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            held.append(fd)
        snapshot = None
        with subprocess.Popen(
            ["restic", "backup", "--force", "--json", str(objects)],
            stdout=subprocess.PIPE, text=True, pass_fds=held,
        ) as process:
            for line in process.stdout:
                event = json.loads(line)
                if event.get("message_type") == "summary":
                    snapshot = event.get("snapshot_id")
            if process.wait() != 0 or not snapshot:
                raise RuntimeError("backup failed; any partial snapshot is NOT approved for restore")
        # Restic can save an incomplete snapshot on exit 3. Only successful
        # backups receive our completion tag; tagging changes the snapshot ID.
        tag = TAG + str(uuid.uuid4())
        restic("tag", "--add", tag, snapshot)
        snapshots = json.loads(restic("snapshots", "--json", "--tag", tag))
        if len(snapshots) != 1:
            raise RuntimeError("cannot identify completed backup")
        return snapshots[0]["id"]


def restore(snapshot, destination):
    if not re.fullmatch(r"[0-9a-f]{64}", snapshot):
        raise ValueError("restore requires the exact full snapshot ID returned by backup")
    metadata = json.loads(restic("cat", "snapshot", snapshot))
    if not any(tag.startswith(TAG) for tag in metadata.get("tags", [])):
        raise ValueError("snapshot is not a completed nanovm offline backup")
    paths = metadata.get("paths", [])
    if len(paths) != 1 or not Path(paths[0]).is_absolute() or Path(paths[0]).name != "objects":
        raise ValueError("unexpected snapshot source paths")
    # mkdir is exclusive, including for empty directories and dangling links.
    # Failure leaves an incomplete root for inspection; never reuse it.
    destination.mkdir(mode=0o700)
    with tempfile.TemporaryDirectory(dir=destination) as staging:
        subprocess.run(
            ["restic", "restore", snapshot, "--target", staging, "--verify"],
            check=True,
        )
        # Absolute backup paths preserve the caller's cwd for every restic
        # credential mechanism. Move only the verified objects subtree.
        os.rename(Path(staging) / paths[0].lstrip("/"), destination / "objects")
    if sorted(p.name for p in destination.iterdir()) != ["objects"]:
        raise ValueError("unexpected snapshot layout; do not start this restore")
    if (destination / "objects").is_symlink() or not (destination / "objects").is_dir():
        raise ValueError("restored objects must be a real directory")
    # The supervisor assigns/preserves worker UIDs. Restic must preserve all
    # numeric owners when run as root; unprivileged drills use their own UID.
    os.chmod(destination, 0o755)
    subprocess.run(["sync", "-f", str(destination)], check=True)
    return str(destination)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    save = commands.add_parser("backup", help="stop all writers first; prints a full snapshot ID")
    save.add_argument("root", type=Path)
    save.add_argument("--stopped", action="store_true", required=True,
                      help="confirm supervisor and all other writers remain stopped")
    load = commands.add_parser("restore", help="restore a trusted snapshot into a nonexistent data root")
    load.add_argument("snapshot")
    load.add_argument("destination", type=Path)
    args = parser.parse_args()
    if not os.environ.get("RESTIC_REPOSITORY"):
        parser.error("set RESTIC_REPOSITORY and restic password credentials in the environment")
    try:
        result = backup(args.root) if args.command == "backup" else restore(args.snapshot, args.destination)
        print(result)
    except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"backup/restore failed: {error}\n")


if __name__ == "__main__":
    main()
