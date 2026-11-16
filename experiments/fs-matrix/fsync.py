"""fsync latency on one filesystem, for the patterns nanovm's commits use.
Usage: python3 -I fsync.py DIR [N]. Prints one JSON line (ms percentiles).

- overwrite: pwrite 4 KiB into an existing file, then fdatasync (an undo record).
- atomic: write a new 16-byte file, fsync it, rename it over the old one,
  fsync the directory (util::write_file_atomic plus a commit's directory sync).
"""
import json, os, sys, time

d, n = sys.argv[1], int(sys.argv[2]) if len(sys.argv) > 2 else 1000

def pct(xs, q):
    xs = sorted(xs)
    return round(xs[min(len(xs) - 1, int(q * len(xs)))] * 1e3, 3)

def timed(f):
    out = []
    for i in range(n):
        t = time.perf_counter()
        f(i)
        out.append(time.perf_counter() - t)
    return {"p50": pct(out, 0.5), "p99": pct(out, 0.99), "mean": round(sum(out) / n * 1e3, 3)}

fd = os.open(os.path.join(d, "log"), os.O_RDWR | os.O_CREAT, 0o644)
os.pwrite(fd, b"\0" * (1 << 20), 0)
os.fsync(fd)
block = os.urandom(4096)
def overwrite(i):
    os.pwrite(fd, block, (i % 256) * 4096)
    os.fdatasync(fd)

dfd = os.open(d, os.O_RDONLY | os.O_DIRECTORY)
def atomic(i):
    tmp = os.path.join(d, "count.tmp")
    f = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    os.write(f, b"%016d" % i)
    os.fsync(f)
    os.close(f)
    os.rename(tmp, os.path.join(d, "count"))
    os.fsync(dfd)

r = {"dir": d, "n": n, "overwrite_fdatasync_ms": timed(overwrite), "atomic_write_rename_ms": timed(atomic)}
os.close(fd); os.close(dfd)
for f in ("log", "count"):
    os.unlink(os.path.join(d, f))
print(json.dumps(r))
