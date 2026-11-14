#!/usr/bin/env python3
"""Benchmark driver for nanovm builds over HTTP (bench/numbers).

Covers what `crates/nanovm/examples/bench.rs` (in-process, no sockets, no
jail) does not: a real `nanovm serve` supervisor with its jailed worker, the
proxy in front of it, node/app/object cold starts, fixed-rate latency with
`oha -q --latency-correction`, CPU per request from /proc, RSS per live
object, sync syscalls per commit (strace), and C's client rate limits.
It also runs each build's own `bench --json` in-process benchmark.

Rounds alternate builds (A B C D A B C D ...). Every measurement is one JSON
line in <out>/raw.jsonl; summarize.py turns that into tables and CSV.

    python3 drive.py --work /home/user/wt/bench/tmp/run --out notes/bench-numbers \\
        --build a:/home/user/wt/bench-a:/home/user/wt/bench/tmp/bin-a:r2 \\
        --build b:/home/user/wt/bench-b:/home/user/wt/bench/tmp/bin-b:r3 \\
        --build c:/home/user/wt/bench-c:/home/user/wt/bench/tmp/bin-c:beta \\
        --rounds 3 [--only phase,phase] [--rates rates.json]

--build NAME:WORKTREE:BINDIR:KIND. BINDIR holds that build's release
`nanovm` (built with --features dev), `examples/dev-registry` and
`examples/bench`, copied out of the shared target dir. KIND picks the config
dialect: r2 (A: proxy `node`, no cgroup/clients keys), r3 (B: proxy `nodes`),
beta (C, D: `cgroup = "off"` required here, `[clients]`, proxy
`hosts`/`signers`). Optional BINDIR/jscounter.wasm is the build's JS app
(js/examples/counter.js through tools/build-app.sh); without it JS is n/a.

Needs: root (the jail sets uids), vm.max_map_count >= 1048576, ulimit -n
20000, oha on PATH or --oha, strace, the composed guests in each worktree's
guests/target/wasm32-wasip2/release/ (the dev registry and bench read them
from there).
"""

import argparse
import http.client
import json
import os
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

NODE = "127.0.0.1:8080"
PROXY = "127.0.0.1:8443"
REGISTRY = "127.0.0.1:5000"
DOH = "127.0.0.1:8053"
TICK = os.sysconf("SC_CLK_TCK")
CONCS = [1, 16, 64, 256]
JS_MANIFEST = {"limits": {"memory_mb": 32, "cpu_ms": 500, "request_timeout_ms": 2000,
                          "idle_timeout_ms": 60000, "disk_mb": 1}}

ARGS = None
OUT = None


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, flush=True)


def record(rec):
    rec["t"] = time.time()
    with open(OUT / "raw.jsonl", "a") as f:
        f.write(json.dumps(rec) + "\n")


# ---------------------------------------------------------------- /proc


def cpu_ticks(pid):
    """utime + stime of every thread of `pid`, in clock ticks."""
    try:
        f = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        return int(f[11]) + int(f[12])
    except (FileNotFoundError, ProcessLookupError, IndexError):
        return 0


def rss_kb(pid):
    try:
        for line in Path(f"/proc/{pid}/status").read_text().splitlines():
            if line.startswith("VmRSS:"):
                return int(line.split()[1])
    except FileNotFoundError:
        pass
    return 0


def children(pid):
    try:
        out = subprocess.run(["ps", "-o", "pid=", "--ppid", str(pid)], capture_output=True, text=True).stdout
        return [int(p) for p in out.split()]
    except Exception:
        return []


def loadavg():
    return float(Path("/proc/loadavg").read_text().split()[0])


def wait_idle(limit=0.5, max_wait=900):
    """Waits for the 1-minute load average to drop below `limit`."""
    t0 = time.time()
    while loadavg() > limit and time.time() - t0 < max_wait:
        time.sleep(10)
    return {"load1": loadavg(), "waited_s": round(time.time() - t0)}


# ---------------------------------------------------------------- processes


class Proc:
    def __init__(self, argv, logf, env=None, cwd=None):
        self.logf = open(logf, "ab")
        self.p = subprocess.Popen(argv, stdout=self.logf, stderr=subprocess.STDOUT, env=env, cwd=cwd,
                                  start_new_session=True)
        self.pid = self.p.pid

    def stop(self, sig=signal.SIGTERM, timeout=15):
        if self.p.poll() is None:
            self.p.send_signal(sig)
            try:
                self.p.wait(timeout)
            except subprocess.TimeoutExpired:
                os.killpg(self.p.pid, signal.SIGKILL)
                self.p.wait()
        self.logf.close()


def kill_strays():
    subprocess.run(["pkill", "-KILL", "-x", "nanovm"], capture_output=True)
    subprocess.run(["pkill", "-KILL", "-x", "dev-registry"], capture_output=True)
    time.sleep(0.3)


def wait_port(addr, timeout=60):
    host, port = addr.split(":")
    t0 = time.time()
    while time.time() - t0 < timeout:
        try:
            socket.create_connection((host, int(port)), 0.2).close()
            return
        except OSError:
            time.sleep(0.02)
    raise RuntimeError(f"{addr} never listened")


def http_req(addr, method, path, headers, body=None, timeout=30):
    host, port = addr.split(":")
    c = http.client.HTTPConnection(host, int(port), timeout=timeout)
    try:
        c.request(method, path, body=body, headers=headers)
        r = c.getresponse()
        return r.status, r.read()
    finally:
        c.close()


# ---------------------------------------------------------------- DoH stub


class DoH(BaseHTTPRequestHandler):
    records = {}

    def do_GET(self):
        q = dict(kv.split("=", 1) for kv in self.path.split("?", 1)[-1].split("&") if "=" in kv)
        name = q.get("name", "")
        txt = self.records.get(name)
        ans = [{"name": name + ".", "type": 16, "TTL": 86400, "data": f'"{txt}"'}] if txt else []
        body = json.dumps({"Status": 0, "Answer": ans}).encode()
        self.send_response(200)
        self.send_header("content-type", "application/dns-json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass


def start_doh(apps):
    DoH.records = {f"_nanovm.{host}": f"app={REGISTRY}/nanovm/{repo}; trust=dev dev" for host, repo in apps.items()}
    srv = ThreadingHTTPServer(("127.0.0.1", int(DOH.split(":")[1])), DoH)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv


# ---------------------------------------------------------------- configs


def node_toml(b, data, clients="off"):
    t = [f'listen = "{NODE}"', f'data_dir = "{data}"', "max_live_objects = 8000", "memory_reservation_mb = 64",
         "epoch_tick_ms = 5", "dev = true",
         # Nothing listens here: exports fail fast and are dropped, the same on every build.
         'telemetry_endpoint = "http://127.0.0.1:4318"']
    if b["kind"] == "beta":
        # This host's cgroup v2 has no controllers: "auto" refuses to start.
        t.append('cgroup = "off"')
        if clients == "off":
            t += ["[clients]", "rate = 0", "app_rate = 0"]
    return "\n".join(t) + "\n"


def proxy_toml(b):
    t = [f'listen = "{PROXY}"', 'publish_listen = "127.0.0.1:8444"', f'resolver = "http://{DOH}/dns-query"',
         "dev = true", 'telemetry_endpoint = "http://127.0.0.1:4318"']
    t.append(f'node = "{NODE}"' if b["kind"] == "r2" else f'nodes = ["{NODE}"]')
    if b["kind"] == "beta":
        t += ['hosts = ["counter.test", "js.test"]', 'signers = ["dev dev"]', "[clients]", "rate = 0", "app_rate = 0"]
    return "\n".join(t) + "\n"


# ---------------------------------------------------------------- oha


def oha(url, conc, *, n=None, z=None, q=None, method="GET", headers=(), body=None, rand=False, host=None):
    argv = [ARGS.oha, "--no-tui", "--output-format", "json", "-c", str(conc)]
    argv += ["-n", str(n)] if n else ["-z", z]
    if q:
        argv += ["-q", str(q), "--latency-correction"]
    if method != "GET":
        argv += ["-m", method]
    for h in headers:
        argv += ["-H", h]
    if host:
        argv += ["--host", host]
    if body:
        argv += ["-D", body]
    if rand:
        argv.append("--rand-regex-url")
    argv.append(url)
    p = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    out = p.stdout.read()
    # wait4 rather than wait: the client's own CPU, reported beside the node's.
    _, _, ru = os.wait4(p.pid, 0)
    p.returncode = 0
    try:
        j = json.loads(out)
    except json.JSONDecodeError:
        raise RuntimeError(f"oha failed: {' '.join(argv)}")
    s = j["summary"]
    codes = {str(k): v for k, v in j.get("statusCodeDistribution", {}).items()}
    total_s = s.get("total") or 1e-9
    lat = j.get("latencyPercentiles", {})
    pct = lambda k: None if lat.get(k) is None else round(lat[k] * 1000, 3)
    return {
        "argv": " ".join(argv), "secs": total_s, "rps_all": s.get("requestsPerSec"),
        "ok": codes.get("200", 0) + codes.get("204", 0), "codes": codes, "errors": j.get("errorDistribution", {}),
        "rps": round((codes.get("200", 0) + codes.get("204", 0)) / total_s, 1),
        "p50_ms": pct("p50"), "p90_ms": pct("p90"), "p99_ms": pct("p99"), "p999_ms": pct("p99.9"),
        "client_cpu_s": ru.ru_utime + ru.ru_stime,
    }


class Cpu:
    """CPU ticks of the supervisor, its workers and the proxy around a run."""

    def __init__(self, sup, proxy=None):
        self.sup, self.proxy = sup, proxy

    def snap(self):
        w = {pid: cpu_ticks(pid) for pid in children(self.sup)}
        return {"sup": cpu_ticks(self.sup), "workers": w, "proxy": cpu_ticks(self.proxy) if self.proxy else 0}

    @staticmethod
    def delta(a, b, requests):
        if not requests:
            return {}
        us = lambda ticks: round(ticks / TICK * 1e6 / requests, 1)
        workers = sum(v - a["workers"].get(pid, 0) for pid, v in b["workers"].items())
        d = {"sup_us": us(b["sup"] - a["sup"]), "worker_us": us(workers), "proxy_us": us(b["proxy"] - a["proxy"])}
        d["node_us"] = round(d["sup_us"] + d["worker_us"], 1)
        d["total_us"] = round(d["node_us"] + d["proxy_us"], 1)
        return d


# ---------------------------------------------------------------- a running stack


class Stack:
    """Dev registry, a node (supervisor + jailed workers) and optionally the proxy."""

    def __init__(self, b, work, data, clients="off", fresh=True):
        self.b, self.work, self.data = b, work, Path(data)
        if fresh and self.data.exists():
            shutil.rmtree(self.data)
        self.cfg = work / f"node-{b['name']}.toml"
        self.cfg.write_text(node_toml(b, self.data, clients))
        self.node = self.proxy = None

    def start_node(self):
        self.node = Proc([str(self.b["bin"] / "nanovm"), "serve", "--config", str(self.cfg)], self.work / "node.log")
        wait_port(NODE)
        return self.node

    def start_proxy(self):
        cfg = self.work / f"proxy-{self.b['name']}.toml"
        cfg.write_text(proxy_toml(self.b))
        self.proxy = Proc([str(self.b["bin"] / "nanovm"), "proxy", "--config", str(cfg)], self.work / "proxy.log")
        wait_port(PROXY)

    def stop(self):
        for p in (self.proxy, self.node):
            if p:
                p.stop()
        self.proxy = self.node = None


def start_registry(b, work):
    (work / "registry.log").unlink(missing_ok=True)
    # dev-registry prints `<guest> <ref>@<digest>` per guest on stdout, into its log.
    reg = Proc([str(b["bin"] / "dev-registry"), REGISTRY], work / "registry.log")
    t0 = time.time()
    refs = {}
    while time.time() - t0 < 60:
        for line in (work / "registry.log").read_text().splitlines():
            parts = line.split()
            if len(parts) == 2 and parts[0] in ("counter", "hostile") and "@sha256:" in parts[1]:
                refs[parts[0]] = parts[1]
        if len(refs) == 2:
            break
        time.sleep(0.1)
    else:
        raise RuntimeError("dev-registry published nothing")
    js = b["bin"] / "jscounter.wasm"
    if js.exists():
        m = work / "jscounter.json"
        m.write_text(json.dumps(JS_MANIFEST))
        out = subprocess.run([str(b["bin"] / "nanovm"), "publish", str(js), f"{REGISTRY}/nanovm/jscounter:1.0.0",
                              "--manifest", str(m)], capture_output=True, text=True)
        if out.returncode == 0:
            refs["jscounter"] = out.stdout.split()[0].replace(":1.0.0@", "@")
        else:
            log("JS publish failed:", out.stderr[-400:])
    return reg, refs


def app_headers(ref):
    return [f"nanovm-app: {ref}", "nanovm-trust: dev dev"]


# ---------------------------------------------------------------- phases


def phase_inproc(b, rnd, work):
    """The build's own in-process benchmark (no sockets, no jail)."""
    env = dict(os.environ, NANOVM_BENCH_DATA=str(work / "bench-data"))
    env.pop("RUST_BACKTRACE", None)
    j = work / f"inproc-{b['name']}-{rnd}.json"
    t0 = time.time()
    out = subprocess.run([str(b["bin"] / "bench"), "--json", str(j), "--cold", "9000"], capture_output=True, text=True,
                         env=env, cwd=b["tree"])
    rec = {"phase": "inproc", "secs": round(time.time() - t0), "stdout": out.stdout[-6000:]}
    if j.exists():
        rec["json"] = json.loads(j.read_text())
    record({**base(b, rnd), **rec})
    if b["kind"] != "r2":
        out = subprocess.run([str(b["bin"] / "bench"), "--writes", "3"], capture_output=True, text=True, env=env,
                             cwd=b["tree"])
        record({**base(b, rnd), "phase": "inproc_writes", "stdout": out.stdout[-6000:]})
    shutil.rmtree(work / "bench-data", ignore_errors=True)
    shutil.rmtree(Path(b["tree"]) / "target/bench-data", ignore_errors=True)


def first_200(addr, path, headers, timeout=120):
    """Polls until the first 200: (seconds since now, non-200 answers seen)."""
    hdr = dict(h.split(": ", 1) for h in headers)
    t0 = time.perf_counter()
    other = {}
    while time.perf_counter() - t0 < timeout:
        try:
            st, _ = http_req(addr, "GET", path, hdr)
            if st == 200:
                return time.perf_counter() - t0, other
            other[st] = other.get(st, 0) + 1
        except (ConnectionRefusedError, ConnectionResetError, http.client.RemoteDisconnected):
            pass
        time.sleep(0.001)
    raise RuntimeError("no 200")


def phase_node_cold(b, rnd, work, refs, n):
    """Process start of `nanovm serve` to the first 200 from the counter app."""
    hdr = app_headers(refs["counter"])
    st = Stack(b, work, work / "data-cold")
    for variant in ("empty_data_dir", "warm_data_dir"):
        samples, odd = [], {}
        # The warm variant's first start follows the empty variant's last, so its data dir is warm.
        for i in range(n):
            if variant == "empty_data_dir" and st.data.exists():
                shutil.rmtree(st.data)
            t0 = time.perf_counter()
            node = Proc([str(b["bin"] / "nanovm"), "serve", "--config", str(st.cfg)], work / "node-cold.log")
            hdr_d = dict(h.split(": ", 1) for h in hdr)
            while True:
                try:
                    s, _ = http_req(NODE, "GET", f"/cold{i}/", hdr_d)
                    if s == 200:
                        break
                    odd[s] = odd.get(s, 0) + 1
                except OSError:
                    pass
                if time.perf_counter() - t0 > 120:
                    raise RuntimeError("node cold: no 200")
                time.sleep(0.001)
            samples.append((time.perf_counter() - t0) * 1000)
            node.stop()
            kill_strays_nodes_only()
        record({**base(b, rnd), "phase": "node_cold", "case": variant, **dist(samples), "non200": odd})
        log(b["name"], "node cold", variant, dist(samples))


def kill_strays_nodes_only():
    subprocess.run(["pkill", "-KILL", "-x", "nanovm"], capture_output=True)
    for _ in range(100):
        if subprocess.run(["pgrep", "-x", "nanovm"], capture_output=True).returncode != 0:
            return
        time.sleep(0.02)


def dist(ms):
    s = sorted(ms)
    q = lambda p: round(s[min(len(s) - 1, int(p * len(s)))], 3)
    return {"n": len(s), "p50_ms": round(statistics.median(s), 3), "p90_ms": q(0.9), "p99_ms": q(0.99),
            "min_ms": round(s[0], 3), "max_ms": round(s[-1], 3)}


def phase_app_cold(b, rnd, work, st, refs, n):
    """First request to the counter app once its worker has been killed (SIGKILL); compile cache warm, or its
    .cwasm files deleted first."""
    hdr = dict(h.split(": ", 1) for h in app_headers(refs["counter"]))
    http_req(NODE, "GET", "/appcold/", hdr)
    for variant in ("cache_warm", "cache_empty"):
        samples, odd = [], {}
        for i in range(n):
            ws = children(st.node.pid)
            for w in ws:
                os.kill(w, signal.SIGKILL)
            for w in ws:
                while Path(f"/proc/{w}").exists():
                    time.sleep(0.005)
            time.sleep(0.2)
            if variant == "cache_empty":
                for f in (st.data / "cache").glob("*/*.cwasm"):
                    f.unlink()
            t0 = time.perf_counter()
            s, _ = http_req(NODE, "GET", f"/appcold{i % 8}/", hdr)
            dt = (time.perf_counter() - t0) * 1000
            if s == 200:
                samples.append(dt)
            else:
                odd[s] = odd.get(s, 0) + 1
            time.sleep(0.2)
        record({**base(b, rnd), "phase": "app_cold", "case": variant, **dist(samples), "non200": odd})
        log(b["name"], "app cold", variant, dist(samples))


def run_case(b, rnd, st, cpu, phase, case, url, conc, *, method="GET", body=None, rand=True, host=None, headers=(),
             z=None, q=None, warm=True, n=None):
    if warm:
        oha(url, conc, z="2s", method=method, headers=headers, body=body, rand=rand, host=host)
    a = cpu.snap()
    r = oha(url, conc, z=z or ARGS.duration, n=n, q=q, method=method, headers=headers, body=body, rand=rand, host=host)
    c = Cpu.delta(a, cpu.snap(), sum(r["codes"].values()))
    rec = {**base(b, rnd), "phase": phase, "case": case, "conc": conc, "q": q, **r, **c}
    record(rec)
    log(b["name"], phase, case, f"c={conc}", f"q={q}" if q else "", f"{r['rps']}/s", f"p50 {r['p50_ms']}",
        f"p99 {r['p99_ms']}", c.get("total_us"), "us/req", r["codes"])
    return rec


def setup_objects(addr, hdr, host=None):
    """1,000 objects o000..o999 of the counter app, each with KV `small` (512 B) and `big` (64 KiB)."""
    h = dict(hh.split(": ", 1) for hh in hdr)
    if host:
        h["host"] = host
    for i in range(1000):
        for key, size in (("small", 512), ("big", 65536)):
            s, _ = http_req(addr, "PUT", f"/o{i:03d}/kv/{key}", h, body=b"x" * size)
            assert s in (200, 204), (s, i, key)


def cases(refs):
    c = [("get_spread", "GET", "/o[0-9]{3}/", True, None),
         ("get_hot", "GET", "/o000/", False, None),
         ("incr_spread", "POST", "/o[0-9]{3}/incr", True, None),
         ("incr_hot", "POST", "/o000/incr", False, None),
         ("kv512_spread", "GET", "/o[0-9]{3}/kv/small", True, None),
         ("kv64k_spread", "GET", "/o[0-9]{3}/kv/big", True, None)]
    if "jscounter" in refs:
        c += [("js_get_spread", "GET", "/j[0-9]{3}/", True, "jscounter"),
              ("js_post_spread", "POST", "/j[0-9]{3}/", True, "jscounter")]
    return c


def phase_throughput(b, rnd, st, refs, cpu, phase="direct", addr=NODE, host_of=None, concs=CONCS, only=None):
    for case, method, path, rand, app in cases(refs):
        if only and case not in only:
            continue
        ref = refs[app or "counter"]
        headers = [] if host_of else app_headers(ref)
        host = host_of(app or "counter") if host_of else None
        for conc in concs:
            run_case(b, rnd, st, cpu, phase, case, f"http://{addr}{path}", conc, method=method, rand=rand,
                     host=host, headers=headers)


def phase_latency(b, rnd, st, refs, cpu, rates):
    for case, method, path, rand, app in cases(refs):
        for frac, rate in rates.get(case, {}).items():
            run_case(b, rnd, st, cpu, "fixed_rate", f"{case}@{frac}", f"http://{NODE}{path}", 64, method=method,
                     rand=rand, headers=app_headers(refs[app or "counter"]), q=rate, z=ARGS.rate_duration)


def phase_object_cold(b, rnd, st, refs, cpu):
    """First request to a new object in a running app (serial, then 64 at once), and warm requests to one live
    object (serial)."""
    hdr = app_headers(refs["counter"])
    run_case(b, rnd, st, cpu, "object_cold", "serial", f"http://{NODE}/n{rnd}[a-z]{{16}}/", 1, headers=hdr, n=2000,
             warm=False)
    run_case(b, rnd, st, cpu, "object_cold", "c64", f"http://{NODE}/m{rnd}[a-z]{{16}}/", 64, headers=hdr, n=6000,
             warm=False)
    run_case(b, rnd, st, cpu, "warm", "serial", f"http://{NODE}/o000/", 1, headers=hdr, n=5000, rand=False)


def phase_density(b, rnd, st, refs):
    """Counter worker RSS as live objects are added (GET on new objects; idle_timeout 60 s, so within it)."""
    hdr = app_headers(refs["counter"])
    worker = [w for w in children(st.node.pid)]
    rows = []
    rows.append({"added": 0, "worker_kb": sum(rss_kb(w) for w in worker), "sup_kb": rss_kb(st.node.pid)})
    for step in range(1, 4):
        oha(f"http://{NODE}/d{rnd}{step}[a-z]{{16}}/", 64, n=2000, headers=hdr, rand=True)
        time.sleep(1)
        rows.append({"added": 2000 * step, "worker_kb": sum(rss_kb(w) for w in children(st.node.pid)),
                     "sup_kb": rss_kb(st.node.pid)})
    per = (rows[-1]["worker_kb"] - rows[0]["worker_kb"]) / rows[-1]["added"]
    record({**base(b, rnd), "phase": "density", "steps": rows, "kb_per_object": round(per, 1),
            "objects_per_gib": round(1024 * 1024 / per) if per > 0 else None})
    log(b["name"], "density", rows, f"{per:.1f} KB/object")


def phase_syncs(b, rnd, st, refs):
    """sync-family syscalls the counter worker makes per request (strace -c), 500 serial requests each."""
    hdr = app_headers(refs["counter"])
    for case, method, path in (("get", "GET", "/o001/"), ("incr", "POST", "/o001/incr"),
                               ("kv_put", "PUT", "/o001/kv/k")):
        w = children(st.node.pid)
        tids = [t for p in w for t in os.listdir(f"/proc/{p}/task")]
        out = st.work / "strace.txt"
        argv = ["strace", "-f", "-c", "-o", str(out), "-e",
                "trace=fsync,fdatasync,sync_file_range,syncfs,sync,msync,renameat,renameat2,rename"]
        for t in tids:
            argv += ["-p", t]
        s = subprocess.Popen(argv, stderr=subprocess.DEVNULL)
        time.sleep(1.0)
        body = None
        if method == "PUT":
            (st.work / "v.bin").write_bytes(b"v" * 100)
            body = str(st.work / "v.bin")
        r = oha(f"http://{NODE}{path}", 1, n=500, method=method, headers=hdr, body=body)
        s.send_signal(signal.SIGINT)
        s.wait(30)
        calls = {}
        for line in out.read_text().splitlines():
            f = line.split()
            if f and f[-1] in ("fsync", "fdatasync", "sync_file_range", "syncfs", "sync", "msync", "renameat",
                               "renameat2", "rename") and f[0][0].isdigit():
                n_calls = int(f[3]) if len(f) >= 5 else int(f[-2])
                calls[f[-1]] = n_calls
        reqs = sum(r["codes"].values())
        record({**base(b, rnd), "phase": "syncs", "case": case, "requests": reqs, "codes": r["codes"],
                "calls": calls, "per_request": {k: round(v / reqs, 2) for k, v in calls.items()}})
        log(b["name"], "syncs", case, calls)


def phase_limits_default(b, rnd, work, refs):
    """C/D only: the node with [clients] at its defaults (rate 100/s per client, app_rate 1,000/s)."""
    st = Stack(b, work, work / "data-limits", clients="default")
    st.start_node()
    cpu = Cpu(st.node.pid)
    hdr = app_headers(refs["counter"])
    oha(f"http://{NODE}/o[0-9]{{3}}/", 16, n=2000, headers=hdr, rand=True)
    time.sleep(3)  # let the warm-up's buckets refill
    for case, method, path in (("get_spread", "GET", "/o[0-9]{3}/"), ("incr_spread", "POST", "/o[0-9]{3}/incr")):
        # Loopback is in `trusted` by default: its own requests are not limited.
        run_case(b, rnd, st, cpu, "limits_default", f"{case}/loopback", f"http://{NODE}{path}", 64, method=method,
                 headers=hdr, z="10s", warm=False)
        time.sleep(3)
        # One client behind a trusted forwarder (a TLS terminator or the proxy): X-Forwarded-For names it.
        run_case(b, rnd, st, cpu, "limits_default", f"{case}/one_client", f"http://{NODE}{path}", 64, method=method,
                 headers=hdr + ["x-forwarded-for: 198.51.100.7"], z="10s", warm=False)
        time.sleep(3)
    # 16 clients at once, each its own address: the app's 1,000/s.
    procs = []
    t0 = time.time()
    for k in range(16):
        argv = [ARGS.oha, "--no-tui", "--output-format", "json", "-c", "4", "-z", "10s", "--rand-regex-url",
                "-H", hdr[0], "-H", hdr[1], "-H", f"x-forwarded-for: 198.51.100.{k + 10}", f"http://{NODE}/o[0-9]{{3}}/"]
        procs.append(subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL))
    codes = {}
    for p in procs:
        j = json.loads(p.communicate()[0])
        for k, v in j.get("statusCodeDistribution", {}).items():
            codes[k] = codes.get(k, 0) + v
    secs = time.time() - t0
    record({**base(b, rnd), "phase": "limits_default", "case": "get_spread/16_clients", "conc": 64, "codes": codes,
            "secs": secs, "rps": round(codes.get("200", 0) / 10, 1)})
    log(b["name"], "limits 16 clients", codes)
    st.stop()


def base(b, rnd):
    return {"build": b["name"], "commit": b["commit"], "round": rnd}


# ---------------------------------------------------------------- main


def one(b, rnd, rates):
    work = Path(ARGS.work) / b["name"]
    work.mkdir(parents=True, exist_ok=True)
    kill_strays()
    record({**base(b, rnd), "phase": "start", **wait_idle(ARGS.idle), "meminfo_free_kb":
            int(Path("/proc/meminfo").read_text().split("MemAvailable:")[1].split()[0])})
    want = lambda p: not ARGS.only or p in ARGS.only
    if want("inproc"):
        phase_inproc(b, rnd, work)
    reg, refs = start_registry(b, work)
    try:
        if want("node_cold"):
            phase_node_cold(b, rnd, work, refs, ARGS.cold_samples)
        st = Stack(b, work, work / "data")
        st.start_node()
        cpu = Cpu(st.node.pid)
        if want("app_cold"):
            phase_app_cold(b, rnd, work, st, refs, ARGS.cold_samples)
        if want("object_cold"):
            phase_object_cold(b, rnd, st, refs, cpu)
        setup_objects(NODE, app_headers(refs["counter"]))
        if want("syncs"):
            phase_syncs(b, rnd, st, refs)
        if want("direct"):
            phase_throughput(b, rnd, st, refs, cpu)
        if want("fixed_rate"):
            phase_latency(b, rnd, st, refs, cpu, rates)
        if want("proxy"):
            doh = start_doh({"counter.test": "counter:latest", "js.test": "jscounter:1.0.0"})
            st.start_proxy()
            cpu_p = Cpu(st.node.pid, st.proxy.pid)
            host_of = lambda app: "counter.test" if app == "counter" else "js.test"
            # C keys an app by the name its record is at, so this may be another app with other volumes.
            setup_objects(PROXY, [], host="counter.test")
            # The same node, objects and cases at 1 and 64 (and 256) concurrent: the proxy's hop.
            phase_throughput(b, rnd, st, refs, cpu_p, phase="proxy", addr=PROXY, host_of=host_of, concs=[1, 64, 256],
                             only={"get_spread", "incr_spread", "kv64k_spread", "js_get_spread"})
            doh.shutdown()
            doh.server_close()
        if want("density"):
            phase_density(b, rnd, st, refs)
        st.stop()
        kill_strays_nodes_only()
        if want("limits_default") and b["kind"] == "beta":
            phase_limits_default(b, rnd, work, refs)
    finally:
        reg.stop()
        kill_strays()
        shutil.rmtree(work / "data", ignore_errors=True)
        shutil.rmtree(work / "data-cold", ignore_errors=True)
        shutil.rmtree(work / "data-limits", ignore_errors=True)


def main():
    global ARGS, OUT
    ap = argparse.ArgumentParser()
    ap.add_argument("--build", action="append", required=True)
    ap.add_argument("--work", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--rounds", type=int, default=3)
    ap.add_argument("--first-round", type=int, default=1)
    ap.add_argument("--only", type=lambda s: set(s.split(",")))
    ap.add_argument("--rates", help="JSON file: {case: {\"50%\": rps, \"80%\": rps}}, the same for every build")
    ap.add_argument("--duration", default="6s")
    ap.add_argument("--rate-duration", default="15s")
    ap.add_argument("--cold-samples", type=int, default=30)
    ap.add_argument("--idle", type=float, default=0.5, help="wait for load1 below this before each build-round")
    ap.add_argument("--oha", default=shutil.which("oha") or "/home/user/wt/tools/oha")
    ARGS = ap.parse_args()
    OUT = Path(ARGS.out)
    OUT.mkdir(parents=True, exist_ok=True)
    assert os.geteuid() == 0, "run as root: the jail sets uids"
    assert int(Path("/proc/sys/vm/max_map_count").read_text()) >= 1048576, "sysctl -w vm.max_map_count=1048576"
    builds = []
    for spec in ARGS.build:
        name, tree, bindir, kind = spec.split(":")
        commit = subprocess.run(["git", "-C", tree, "rev-parse", "--short", "HEAD"], capture_output=True,
                                text=True).stdout.strip()
        builds.append({"name": name, "tree": tree, "bin": Path(bindir), "kind": kind, "commit": commit})
    rates = json.loads(Path(ARGS.rates).read_text()) if ARGS.rates else {}
    record({"phase": "machine", "cpu": subprocess.run(["sh", "-c", "grep -m1 'model name' /proc/cpuinfo"],
                                                      capture_output=True, text=True).stdout.strip(),
            "nproc": os.cpu_count(), "uname": " ".join(os.uname()), "mem": Path("/proc/meminfo").read_text()[:80],
            "builds": [{k: str(v) for k, v in b.items()} for b in builds], "argv": sys.argv})
    for rnd in range(ARGS.first_round, ARGS.first_round + ARGS.rounds):
        for b in builds:
            log(f"round {rnd} build {b['name']} ({b['commit']})")
            one(b, rnd, rates)


if __name__ == "__main__":
    main()
