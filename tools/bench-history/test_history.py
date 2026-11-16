"""Tests for history.py on synthetic bench.rs output.

    python3 -m unittest discover -s tools/bench-history -v
"""

import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace as NS

import history


def run(rps, p99, failed=0):
    return {"requests": 1000, "failed": failed, "failure_reasons": {}, "elapsed_ms": 1e6 / rps,
            "rps": rps, "p50_ms": p99 / 4, "p99_ms": p99, "p999_ms": p99 * 2}


def bench(rps=1000.0, p99=4.0):
    return {"cold": {"run": run(rps, p99), "live": 100, "rss_mb": 50.0, "kb_per_object": 90},
            "warm_get": run(rps * 10, p99), "warm_serial": run(rps, p99 / 10), "warm_incr": run(rps, p99),
            "warm_incr_serial": run(rps / 2, p99), "recover": run(rps, p99), "churn": run(rps, p99),
            "neighbours": {"baseline": run(rps * 8, p99), "under_attack": run(rps * 3, p99 * 2),
                           "attacks": 100, "killed": 90}}


class History(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.path = Path(self.dir.name) / "h.jsonl"

    def tearDown(self):
        self.dir.cleanup()

    def add(self, commit, b, cpu="test"):
        entry = {"commit": commit, "recorded": "2026-11-08T00:00:00+00:00", "mode": "quick", "profile": "release",
                 "machine": {"cpu": cpu, "cores": 4}, "metrics": history.metrics(b)}
        with open(self.path, "a") as f:
            f.write(json.dumps(entry) + "\n")

    def call(self, fn, **kw):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = fn(NS(history=str(self.path), **kw))
        return code, out.getvalue()

    def test_metrics_flatten_every_scenario(self):
        m = history.metrics(bench())
        self.assertEqual(m["cold.rps"], 1000.0)
        self.assertEqual(m["cold.kb_per_object"], 90)
        self.assertEqual(m["neighbours_attacked.p99_ms"], 8.0)
        self.assertEqual(len([k for k in m if k.endswith(".rps")]), len(history.RUNS))

    def test_record_ingests_bench_json_with_commit_and_machine(self):
        src = Path(self.dir.name) / "b.json"
        src.write_text(json.dumps(bench()))
        with contextlib.redirect_stdout(io.StringIO()):
            history.record(NS(history=str(self.path), bench_json=str(src), profile="dev"))
        [entry] = history.load(self.path)
        self.assertRegex(entry["commit"], r"^[0-9a-f]{40}$")
        self.assertEqual((entry["profile"], entry["mode"]), ("dev", "quick"))
        self.assertIn("cores", entry["machine"])
        self.assertEqual(entry["bench"], bench())

    def test_compare_flags_only_changes_past_the_threshold(self):
        for i in range(5):
            self.add(f"c{i}", bench(rps=1000 + i, p99=4.0))
        self.add("new", bench(rps=900, p99=4.1))  # 10% fewer requests/s, 2.5% slower p99
        code, out = self.call(history.compare, threshold=0.05, baseline=5)
        self.assertEqual(code, 1)
        self.assertIn("| cold.rps | 1002 | 1000–1004 | 900 | -10.2% | **worse** |", out)
        self.assertNotIn("p99_ms | 4 | 4–4 | 4.1 | +2.5% | **worse**", out)
        self.add("newer", bench(rps=1001, p99=4.0))
        code, out = self.call(history.compare, threshold=0.05, baseline=5)
        self.assertEqual(code, 0, out)

    def test_compare_uses_only_runs_on_the_same_machine_type(self):
        self.add("fast", bench(rps=2000), cpu="other")
        self.add("a", bench(rps=1000))
        self.add("b", bench(rps=990))
        self.assertEqual(self.call(history.compare, threshold=0.05, baseline=5)[0], 0)
        code, out = self.call(history.compare, threshold=0.05, baseline=5, any_machine=True)
        self.assertEqual(code, 1, out)

    def test_runs_of_one_commit_use_their_median_and_one_commit_is_not_compared(self):
        self.add("a", bench(rps=1000))
        self.assertEqual(self.call(history.compare, threshold=0.05, baseline=5)[0], 0)
        for rps in (500, 1000, 1010):
            self.add("b", bench(rps=rps))
        self.assertEqual(history.by_commit(history.load(self.path))["b"]["cold.rps"], 1000)

    def test_chart_and_export(self):
        for i, c in enumerate("abc"):
            self.add(c, bench(rps=1000 + 10 * i))
        out = Path(self.dir.name) / "t.html"
        self.call(history.chart, out=str(out))
        page = out.read_text()
        self.assertIn("<b>cold.rps</b> (higher is better)", page)
        self.assertEqual(page.count("<polyline"), len([k for k in history.metrics(bench())
                                                       if k.rsplit(".", 1)[1] in history.DIRECTION]))
        _, rows = self.call(history.export)
        rows = [json.loads(r) for r in rows.splitlines()]
        self.assertEqual(len(rows), 3 * len(history.RUNS))
        # Exactly the fields the nice-to-haves summarizer requires.
        self.assertEqual(set(rows[0]), {"run", "variant", "workload", "path", "environment", "provenance", "requests",
                                        "errors", "seconds", "p99_ms", "expected_spans", "received_spans"})
        self.assertEqual(rows[0]["seconds"], 1.0)


if __name__ == "__main__":
    unittest.main()
