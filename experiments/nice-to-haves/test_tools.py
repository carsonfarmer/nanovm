"""Public tool contracts; HTTP tests use only a disposable loopback server."""

from copy import deepcopy
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import unittest

from http_operation import attempt
from toolkit import capacity, check_links, inspect_wasm, summarize

HERE = Path(__file__).resolve().parent


class OfflineTools(unittest.TestCase):
    def setUp(self):
        self.inputs = json.loads((HERE / "fixtures/capacity.json").read_text())
        self.rows = [json.loads(line) for line in (HERE / "fixtures/benchmarks.jsonl").read_text().splitlines()]

    def test_capacity_units_and_limiting_budget(self):
        result = capacity(self.inputs)
        self.assertEqual(result["combined_rps_ceiling"], 14000)
        self.assertEqual(result["resident_objects_estimate"], 7168)
        self.assertEqual(result["mean_inflight_at_target"], 50)
        self.inputs["collector_cpu_us"]["value"] = 100
        self.assertEqual(capacity(self.inputs)["combined_rps_ceiling"], 7000)
        self.assertFalse(capacity(self.inputs)["target_within_cpu_estimate"])
        self.inputs["target_rps"]["value"] = 7000
        self.assertTrue(capacity(self.inputs)["target_within_cpu_estimate"])

    def test_capacity_rejects_unlabelled_or_invalid_inputs(self):
        for key, field, value in [
            ("application_cpu_us", "value", 0), ("target_utilization", "value", 1.01),
            ("target_utilization", "value", 0), ("target_rps", "value", float("nan")),
            ("application_cores", "value", True), ("fixed_memory_mib", "value", 1025),
            ("collector_cpu_us", "source", ""), ("target_rps", "basis", "guess"),
        ]:
            with self.subTest(key=key, field=field, value=value):
                changed = deepcopy(self.inputs)
                changed[key][field] = value
                with self.assertRaises(ValueError):
                    capacity(changed)

    def test_summary_weights_time_not_run_rates(self):
        group = summarize(self.rows)["groups"][0]
        self.assertEqual(group["aggregate_rps"], 2500)
        self.assertEqual(group["successful_rps"], 2477.5)
        self.assertEqual(group["median_run_rps"], 2000)
        self.assertEqual(group["run_p99_ms_range"], [8, 12])
        self.assertFalse(group["every_run_span_count_matches"])

    def test_summary_never_merges_paths_environments_or_provenance(self):
        rows = [self.rows[0]]
        for key, value in [("path", "in-process"), ("environment", "different"), ("provenance", "measured")]:
            row = dict(rows[0])
            row[key] = value
            rows.append(row)
        self.assertEqual(len(summarize(rows)["groups"]), 4)

    def test_summary_cannot_hide_loss_with_extra_spans_in_another_run(self):
        self.rows[0]["received_spans"] += 1
        group = summarize(self.rows)["groups"][0]
        self.assertEqual(group["expected_spans"], group["received_spans"])
        self.assertFalse(group["every_run_span_count_matches"])

    def test_summary_rejects_duplicates_bad_counts_and_nonfinite_duration(self):
        with self.assertRaises(ValueError):
            summarize([self.rows[0], self.rows[0]])
        for key, value in [("requests", 1.5), ("errors", 1001), ("seconds", 0), ("seconds", float("inf"))]:
            row = dict(self.rows[0])
            row[key] = value
            with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                summarize([row])

    def test_wasm_accounts_for_headers_and_custom_sections(self):
        # Custom section name 'x', 128 data bytes; length needs two LEB bytes.
        blob = b"\0asm\x0d\0\x01\0" + b"\0\x82\x01\x01x" + b"z" * 128
        result = inspect_wasm(blob)
        self.assertEqual(result["kind"], "component")
        self.assertEqual(result["bytes"], 141)
        self.assertEqual(result["custom_section_bytes"], 133)
        self.assertEqual(result["sections"][0]["custom_name"], "x")
        self.assertEqual(inspect_wasm(b"\0asm\x01\0\0\0")["kind"], "core-module")

    def test_wasm_rejects_truncated_overflowing_and_invalid_custom_names(self):
        header = b"\0asm\x01\0\0\0"
        for tail in [b"\x01", b"\x01\x80", b"\x01\x80\x80\x80\x80\x10", b"\x01\x05x", b"\0\x02\x05x", b"\0\x02\x01\xff"]:
            with self.subTest(tail=tail), self.assertRaises(ValueError):
                inspect_wasm(header + tail)

    def test_links_handles_spaces_fences_missing_and_escaping_targets(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "has space.md").write_text("# Exists\n")
            (root / "escape").symlink_to(root.parent)
            doc = root / "readme.md"
            doc.write_text('[ok](has%20space.md#unvalidated)\n[missing](missing.md)\n'
                           '[escape](escape/out.md)\n[remote](https://example.invalid)\n'
                           '[anchor](#local)\n```md\n[fake](ignored.md)\n```\n')
            result = check_links(root, [doc])
            self.assertEqual(result["checked"], 3)
            self.assertEqual(result["skipped_external_or_anchor_only"], 2)
            self.assertEqual([p["line"] for p in result["problems"]], [2, 3])

    def test_cli_outputs_json_and_invalid_input_exits_two(self):
        result = subprocess.run([sys.executable, str(HERE / "toolkit.py"), "summarize",
                                 str(HERE / "fixtures/benchmarks.jsonl")], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["groups"][0]["aggregate_rps"], 2500)
        result = subprocess.run([sys.executable, str(HERE / "toolkit.py"), "capacity",
                                 str(HERE / "fixtures/benchmarks.jsonl")], capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertNotIn("Traceback", result.stderr)


class HttpExample(unittest.TestCase):
    def setUp(self):
        self.received = []
        received = self.received

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                body = self.rfile.read(int(self.headers["Content-Length"]))
                received.append((self.path, self.headers["Idempotency-Key"], body))
                if self.path == "/disconnect":
                    self.connection.shutdown(socket.SHUT_RDWR)
                    self.connection.close()
                    return
                if self.path == "/broken-chunk":
                    self.send_response(200)
                    self.send_header("Transfer-Encoding", "chunked")
                    self.end_headers()
                    self.wfile.write(b"not-a-size\r\n")
                    return
                if self.path == "/short-body":
                    self.send_response(200)
                    self.send_header("Content-Length", "100")
                    self.end_headers()
                    self.wfile.write(b"short")
                    return
                status = {"/redirect": 302, "/redirect307": 307, "/unavailable": 503}.get(self.path, 200)
                self.send_response(status)
                self.send_header("Location", "/should-not-receive")
                self.end_headers()
                self.wfile.write(b"abcdefgh")

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever)
        self.thread.start()
        self.url = f"http://127.0.0.1:{self.server.server_port}"

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()

    def test_posts_exact_body_and_stable_operation_id_with_bounded_response(self):
        result = attempt(self.url + "/apply", "op-123", b'{"amount":7}', response_limit=3)
        self.assertEqual(self.received, [("/apply", "op-123", b'{"amount":7}')])
        self.assertEqual(result["body_utf8"], "abc")
        self.assertTrue(result["body_truncated"])
        self.assertEqual(result["status"], 200)

    def test_does_not_follow_redirects_or_retry_http_errors(self):
        for path, status in [("/redirect", 302), ("/redirect307", 307), ("/unavailable", 503)]:
            result = attempt(self.url + path, "stable", b"{}")
            self.assertEqual(result["status"], status)
        self.assertEqual([r[0] for r in self.received], ["/redirect", "/redirect307", "/unavailable"])

    def test_disconnect_and_malformed_response_are_unknown_without_retry(self):
        for path in ("/disconnect", "/broken-chunk", "/short-body"):
            result = attempt(self.url + path, "stable", b"{}")
            self.assertEqual(result["outcome"], "unknown")
            self.assertNotIn("body_utf8", result)
        self.assertEqual(len(self.received), 3)

    def test_invalid_ids_and_urls_never_send(self):
        for url, op in [(self.url, "x\r\nInjected: yes"), ("file:///etc/passwd", "ok"),
                        ("http://user:password@localhost/", "ok")]:
            with self.assertRaises(ValueError):
                attempt(url, op, b"{}")
        self.assertEqual(self.received, [])


if __name__ == "__main__":
    unittest.main()
