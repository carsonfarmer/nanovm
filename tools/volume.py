#!/usr/bin/env python3
"""Export one object's /data as a TAR and import it into a fresh data root.

    tools/volume.py list ROOT [APP]
    tools/volume.py export ROOT APP OBJECT OUT.tar             # offline
    tools/volume.py export ROOT APP OBJECT OUT.tar --committed # from local capture
    tools/volume.py import IN.tar ROOT [--app APP] [--object OBJECT]

ROOT is a node's data_dir; APP is the app's directory under ROOT/objects
(`list` shows them); OBJECT is the object id.

The format is the one local capture writes (W-9, crates/nanovm/src/committed.rs):
a TAR whose first member is `nanovm.json` ({"format": 1, "app", "object",
"sequence", "parent"}), then `data/` and the volume's directories and regular
files under it. A committed version (`ROOT/objects/APP/~committed/OBJECT/N.tar`)
is already an export; an offline export has sequence 0 and no parent.

Offline export holds the app's storage lock (as scripts/backup.py does and as
the node does while the app's worker runs), so it fails while the app is
running: stop the node, or use --committed, which copies the newest committed
version and needs no lock (versions are immutable once published). It refuses
an object with an undo log: its volume holds a request's uncommitted changes
until the node next starts the object and rolls them back.

Import writes into ROOT/objects/APP/OBJECT, which must not exist, nor may the
object have a journal or capture history there. It holds the app's lock,
extracts into a staging directory beside the volume, syncs every file and
directory, then renames it into place, so the object appears whole or not at
all. Run as root, it gives the files the app directory's owner (the app's uid).
Usage counts against the app's disk_mb and max_files when the object starts.
"""

import argparse
import fcntl
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import sys
import tarfile
import uuid

FORMAT = 1
IDENTITY = "nanovm.json"
MAX_IDENTITY = 4096
VERSION_NAME = re.compile(r"[0-9]{20}\.tar")


class Refused(Exception):
    pass


def valid_name(s):
    # table::valid_name: [A-Za-z0-9_.-]{1,128}, not starting with "."
    return bool(re.fullmatch(r"[A-Za-z0-9_.-]{1,128}", s)) and not s.startswith(".")


def app_dir(root, app, create=False):
    if "/" in app or app in ("", ".", "..") or app.startswith("~"):
        raise Refused(f"not an app directory name: {app!r}")
    objects = Path(root) / "objects"
    if objects.is_symlink():
        raise Refused(f"{objects} must be a real directory")
    path = objects / app
    if create:
        path.mkdir(parents=True, exist_ok=True)
    if path.is_symlink() or not path.is_dir():
        raise Refused(f"{path} is not an app directory")
    return path


def lock(path):
    """The app's storage lock: flock on its directory, as App::load takes it."""
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        os.close(fd)
        raise Refused(f"{path} is locked: the app's worker is running. Stop the node, "
                      "or export a committed version (--committed) if capture is on")
    return fd


def fsync_path(path, directory=False):
    fd = os.open(path, (os.O_RDONLY | os.O_DIRECTORY) if directory else os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def identity(app, obj, sequence=0, parent=""):
    return json.dumps({"format": FORMAT, "app": app, "object": obj, "sequence": sequence,
                       "parent": parent}, separators=(",", ":")).encode()


def entries(volume):
    """The volume's directories and files, parents first; nothing else."""
    out = []
    pending = [volume]
    while pending:
        d = pending.pop()
        for e in sorted(os.scandir(d), key=lambda e: e.name):
            st = e.stat(follow_symlinks=False)
            if stat.S_ISDIR(st.st_mode):
                pending.append(Path(e.path))
            elif not stat.S_ISREG(st.st_mode):
                raise Refused(f"{e.path} is neither a file nor a directory")
            out.append(Path(e.path))
    return out


def write_atomically(out, write):
    out = Path(out).absolute()
    tmp = out.with_name(f".{out.name}.{uuid.uuid4().hex}.tmp")
    try:
        with open(tmp, "xb") as f:
            write(f)
            f.flush()
            os.fsync(f.fileno())
        os.rename(tmp, out)
    finally:
        tmp.unlink(missing_ok=True)
    fsync_path(out.parent, directory=True)


def export_offline(root, app, obj, out):
    if not valid_name(obj):
        raise Refused(f"not an object id: {obj!r}")
    adir = app_dir(root, app)
    fd = lock(adir)
    try:
        journal = adir / "~journal" / obj
        pending = [p.name for p in (journal / "log", journal / "capture") if p.exists()]
        if pending:
            raise Refused(f"{journal} holds {' and '.join(pending)}: the volume has a request's uncommitted "
                          "changes. Start the node and send the object a request (it rolls them back), then export")
        volume = adir / obj
        if volume.is_symlink() or not volume.is_dir():
            raise Refused(f"{volume} is not an object volume")
        files = entries(volume)

        def add(tar, path, name):
            # By hand: tarfile.add would turn a second name for an inode
            # into a link member, which neither capture nor import has.
            st = os.lstat(path)
            info = tarfile.TarInfo(name)
            info.mode, info.mtime = stat.S_IMODE(st.st_mode), int(st.st_mtime)
            if stat.S_ISDIR(st.st_mode):
                info.type = tarfile.DIRTYPE
                tar.addfile(info)
            else:
                info.size = st.st_size
                with open(path, "rb") as f:
                    tar.addfile(info, f)

        def write(f):
            with tarfile.open(fileobj=f, mode="w", format=tarfile.GNU_FORMAT) as tar:
                meta = identity(app, obj)
                info = tarfile.TarInfo(IDENTITY)
                info.size, info.mode = len(meta), 0o600
                tar.addfile(info, io.BytesIO(meta))
                add(tar, volume, "data")
                for p in files:
                    add(tar, p, str(PurePosixPath("data", p.relative_to(volume).as_posix())))
        write_atomically(out, write)
        return len(files)
    finally:
        os.close(fd)


def read_identity(tar):
    """The archive's identity, and its member (iterating `tar` yields it again)."""
    first = tar.next()
    if first is None or first.name != IDENTITY or not first.isfile() or first.size > MAX_IDENTITY:
        raise Refused(f"not a nanovm volume export: the first member must be {IDENTITY}")
    meta = json.loads(tar.extractfile(first).read())
    if not isinstance(meta, dict) or sorted(meta) != ["app", "format", "object", "parent", "sequence"] \
            or meta["format"] != FORMAT:
        raise Refused(f"unknown {IDENTITY}: {meta}")
    return meta, first


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 16), b""):
            h.update(chunk)
    return h.hexdigest()


def export_committed(root, app, obj, out, sequence=None):
    if not valid_name(obj):
        raise Refused(f"not an object id: {obj!r}")
    history = app_dir(root, app) / "~committed" / obj
    names = sorted(p.name for p in history.iterdir()) if history.is_dir() else []
    if not names:
        raise Refused(f"{history} has no committed versions (is [capture] on?)")
    if any(not VERSION_NAME.fullmatch(n) for n in names) or [int(n[:20]) for n in names] != list(range(1, len(names) + 1)):
        raise Refused(f"{history} is not a contiguous history of versions")
    n = len(names) if sequence is None else sequence
    if not 1 <= n <= len(names):
        raise Refused(f"no version {n}: there are {len(names)}")
    version = history / f"{n:020}.tar"
    with tarfile.open(version) as tar:
        meta, _ = read_identity(tar)
    if (meta["app"], meta["object"], meta["sequence"]) != (app, obj, n):
        raise Refused(f"{version} names {meta}, not {app}/{obj} version {n}")
    if n > 1 and meta["parent"] != sha256(history / f"{n - 1:020}.tar"):
        raise Refused(f"{version}'s parent digest does not match version {n - 1}")

    def write(f):
        with open(version, "rb") as src:
            shutil.copyfileobj(src, f)
    write_atomically(out, write)
    return n


def member_path(m):
    p = PurePosixPath(m.name)
    if p.is_absolute() or ".." in p.parts or p.parts[:1] != ("data",) or str(p) != m.name.rstrip("/"):
        raise Refused(f"unsafe or foreign path in the archive: {m.name!r}")
    return p.relative_to("data")


def import_volume(archive, root, app=None, obj=None):
    with tarfile.open(archive) as tar:
        meta, first = read_identity(tar)
        app, obj = app or meta["app"], obj or meta["object"]
        if not valid_name(obj):
            raise Refused(f"not an object id: {obj!r}")
        Path(root).mkdir(parents=True, exist_ok=True)
        adir = app_dir(root, app, create=True)
        fd = lock(adir)
        staging = adir / f".import-{uuid.uuid4().hex}"
        try:
            for p in (adir / obj, adir / "~journal" / obj, adir / "~committed" / obj):
                if os.path.lexists(p):
                    raise Refused(f"{p} exists: import only into a fresh place")
            owner = os.stat(adir) if os.geteuid() == 0 else None
            staging.mkdir(mode=0o755)
            seen, files, size, modes = set(), 0, 0, {}
            for m in tar:
                if m is first:
                    continue
                rel = member_path(m)
                if rel in seen:
                    raise Refused(f"{m.name} appears twice")
                seen.add(rel)
                dest = staging / rel if rel != PurePosixPath(".") else staging
                if m.isdir():
                    if rel != PurePosixPath("."):
                        dest.mkdir(mode=0o755)
                elif m.isfile():
                    fd_out = os.open(dest, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o644)
                    with os.fdopen(fd_out, "wb") as f:
                        shutil.copyfileobj(tar.extractfile(m), f)
                        f.flush()
                        os.fsync(f.fileno())
                    size += m.size
                else:
                    raise Refused(f"{m.name}: only files and directories are allowed")
                if rel != PurePosixPath("."):
                    files += 1
                if m.isfile():
                    # The owner (the app's uid) keeps read and write.
                    os.chmod(dest, (m.mode & 0o777) | 0o600)
                else:
                    modes[dest] = (m.mode & 0o777) | 0o700
                if owner:
                    os.lchown(dest, owner.st_uid, owner.st_gid)
            if PurePosixPath(".") not in seen:
                raise Refused("the archive has no data/ directory")
            # Directories last, deepest first: their modes once their entries
            # exist, then a sync of each.
            for d in sorted(modes, key=lambda p: -len(p.parts)):
                os.chmod(d, modes[d])
                fsync_path(d, directory=True)
            os.rename(staging, adir / obj)
            for d in (adir, adir.parent, adir.parent.parent):
                fsync_path(d, directory=True)
            return app, obj, files, size
        finally:
            if staging.exists():
                shutil.rmtree(staging)
            os.close(fd)


def list_root(root, only=None):
    objects = Path(root) / "objects"
    for adir in sorted(objects.iterdir()) if objects.is_dir() else []:
        if only and adir.name != only:
            continue
        print(adir.name)
        for vol in sorted(p for p in adir.iterdir() if valid_name(p.name) and p.is_dir()):
            nbytes = nfiles = 0
            for dirpath, dirnames, filenames in os.walk(vol):
                nfiles += len(dirnames) + len(filenames)
                nbytes += sum(os.lstat(os.path.join(dirpath, f)).st_size for f in filenames)
            notes = []
            if (adir / "~journal" / vol.name / "log").exists():
                notes.append("undo log pending")
            history = adir / "~committed" / vol.name
            if history.is_dir():
                notes.append(f"{len(list(history.iterdir()))} committed versions")
            print(f"  {vol.name}: {nfiles} entries, {nbytes} bytes" + (f" ({', '.join(notes)})" if notes else ""))


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="command", required=True)
    ls = sub.add_parser("list", help="apps and objects under a data root")
    ls.add_argument("root", type=Path)
    ls.add_argument("app", nargs="?")
    ex = sub.add_parser("export", help="one object's /data to a TAR")
    ex.add_argument("root", type=Path)
    ex.add_argument("app")
    ex.add_argument("object")
    ex.add_argument("out", type=Path)
    ex.add_argument("--committed", nargs="?", const=0, type=int, metavar="SEQUENCE",
                    help="copy a committed version (the newest unless SEQUENCE); needs no lock")
    im = sub.add_parser("import", help="a TAR into a fresh object volume")
    im.add_argument("archive", type=Path)
    im.add_argument("root", type=Path)
    im.add_argument("--app", help="the app directory to import into (default: the archive's)")
    im.add_argument("--object", help="the object id to import as (default: the archive's)")
    args = p.parse_args()
    try:
        if args.command == "list":
            list_root(args.root, args.app)
        elif args.command == "export" and args.committed is not None:
            n = export_committed(args.root, args.app, args.object, args.out, args.committed or None)
            print(f"exported committed version {n} of {args.app}/{args.object} to {args.out}")
        elif args.command == "export":
            n = export_offline(args.root, args.app, args.object, args.out)
            print(f"exported {args.app}/{args.object} ({n} entries) to {args.out}")
        else:
            app, obj, files, size = import_volume(args.archive, args.root, args.app, args.object)
            print(f"imported {files} entries, {size} bytes into {args.root}/objects/{app}/{obj}")
    except (Refused, OSError, tarfile.TarError, json.JSONDecodeError) as e:
        sys.exit(f"volume: {e}")


if __name__ == "__main__":
    main()
