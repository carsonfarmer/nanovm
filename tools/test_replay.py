"""Tests for replay.py.  python3 -m unittest discover -s tools -p test_replay.py -v"""

import importlib.util
import io
import json
from pathlib import Path
import random
import tempfile
import threading
import http.server
import unittest
import urllib.request

spec = importlib.util.spec_from_file_location("replay", Path(__file__).with_name("replay.py"))
replay = importlib.util.module_from_spec(spec)
spec.loader.exec_module(replay)


def backend(version):
    """An app with an object per path prefix: POST /{o}/incr, GET /{o}/; v2 counts in tens."""
    counts = {}

    class H(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def answer(self, status, body, error=None):
            data = json.dumps(body).encode()
            self.send_response(status)
            if error:
                self.send_header("x-nanovm-error", error)
            self.send_header("content-length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self):
            obj = self.path.split("/")[1]
            if self.path.endswith("/trap"):
                return self.answer(500, {"error": "trap"}, "trap")
            self.answer(200, {"object": obj, "count": counts.get(obj, 0), "host": self.headers["host"],
                              "instance": random.random()})

        def do_POST(self):
            obj = self.path.split("/")[1]
            n = int(self.headers.get("content-length") or 0)
            step = int(self.rfile.read(n) or b"1") * (10 if version == 2 else 1)
            counts[obj] = counts.get(obj, 0) + step
            self.answer(200, {"count": counts[obj]})

        def log_message(self, *a):
            pass

    s = http.server.ThreadingHTTPServer(("127.0.0.1", 0), H)
    threading.Thread(target=s.serve_forever, daemon=True).start()
    return s, f"http://127.0.0.1:{s.server_address[1]}"


class ReplayTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        self.log = Path(self.dir.name) / "session.jsonl"
        self.v1, self.v1_url = backend(1)
        self.addCleanup(self.v1.server_close)
        self.addCleanup(self.v1.shutdown)
        rec = replay.record(self.v1_url, "127.0.0.1:0", str(self.log))
        threading.Thread(target=rec.serve_forever, daemon=True).start()
        self.addCleanup(rec.server_close)
        self.addCleanup(rec.shutdown)
        url = f"http://127.0.0.1:{rec.server_address[1]}"

        def send(method, path, body=None):
            req = urllib.request.Request(url + path, data=body, method=method, headers={"Host": "app.localhost"})
            try:
                with urllib.request.urlopen(req) as r:
                    return r.status, r.read()
            except urllib.error.HTTPError as e:
                return e.code, e.read()
        self.assertEqual(send("POST", "/a/incr", b"1")[0], 200)
        self.assertEqual(send("POST", "/a/incr", b"2")[0], 200)
        status, body = send("GET", "/a/")
        self.assertEqual((status, json.loads(body)["count"], json.loads(body)["host"]), (200, 3, "app.localhost"))
        self.assertEqual(send("GET", "/a/trap")[0], 500)

    def run_replay(self, url, **kw):
        out = io.StringIO()
        return replay.replay(str(self.log), url, out=out, **kw), out.getvalue()

    def test_records_requests_and_answers(self):
        entries = [json.loads(line) for line in self.log.read_text().splitlines()]
        self.assertEqual([(e["method"], e["path"], e["status"], e["error"]) for e in entries],
                         [("POST", "/a/incr", 200, None), ("POST", "/a/incr", 200, None), ("GET", "/a/", 200, None),
                          ("GET", "/a/trap", 500, "trap")])
        self.assertEqual(entries[0]["headers"]["Host"], "app.localhost")

    def test_same_version_on_fresh_objects_answers_the_same(self):
        differ, out = self.run_replay(self.v1_url, prefix="r1-", ignore=["instance", "object"])
        self.assertEqual(differ, 0, out)
        # Without --ignore the random field differs; on the old objects their state does.
        self.assertEqual(self.run_replay(self.v1_url, prefix="r2-")[0], 1)
        differ, out = self.run_replay(self.v1_url, ignore=["instance", "object"])
        self.assertEqual(differ, 3, out)

    def test_a_new_version_is_diffed(self):
        v2, url = backend(2)
        self.addCleanup(v2.server_close)
        self.addCleanup(v2.shutdown)
        differ, out = self.run_replay(url, prefix="r1-", ignore=["instance", "object"], host="new.localhost")
        self.assertEqual(differ, 3, out)
        self.assertIn("DIFF POST /r1-a/incr", out)
        self.assertIn("4 requests replayed, 3 answered differently", out)


if __name__ == "__main__":
    unittest.main()
