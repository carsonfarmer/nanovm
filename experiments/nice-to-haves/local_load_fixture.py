#!/usr/bin/env python3
"""Exercise pinned oha against a disposable loopback fixture, never a supplied URL."""

import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import socket
import subprocess
import threading

from benchmark_import import OHA_VERSION, import_oha, manifest, pinned_tool
from toolkit import summarize


def run_fixture(oha, repo, output):
    oha = Path(oha).resolve(strict=True)
    pinned_tool(oha, OHA_VERSION)
    output = Path(output)
    output.mkdir()  # Refuse existing directories: raw evidence is never overwritten.
    rows, observations = [], []
    for case in ("success", "http-error", "mixed", "transport-error"):
        received = []
        lock = threading.Lock()

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_GET(self):
                with lock:
                    index = len(received)
                    status = {"success": 200, "http-error": 503, "transport-error": None,
                              "mixed": (200, 503, None)[index % 3]}[case]
                    received.append(status)
                if status is None:
                    self.connection.shutdown(socket.SHUT_RDWR)
                    self.connection.close()
                    return
                self.send_response(status)
                self.send_header("Content-Length", "2")
                self.end_headers()
                self.wfile.write(b"ok")

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever)
        thread.start()
        try:
            result = subprocess.run([
                str(oha), "--no-tui", "--output-format", "json", "-n", "12", "-c", "2",
                "-t", "1s", "-r", "0", f"http://127.0.0.1:{server.server_port}/fixture",
            ], capture_output=True, check=True, timeout=20, env={"PATH": os.defpath})
        finally:
            server.shutdown()
            server.server_close()
            thread.join()
        raw_path = output / f"{case}.oha.json"
        raw_path.write_bytes(result.stdout)
        scenario = {"run": case, "variant": "python-loopback-fixture", "workload": case,
                    "path": "http-node", "environment": "local-fixture-n12-c2-no-runtime",
                    "provenance": "measured", "planned_requests": 12, "accepted_statuses": [200],
                    "expected_spans": 0, "received_spans": 0}
        metadata = manifest(repo, raw_path, scenario, oha)
        (output / f"{case}.manifest.json").write_text(json.dumps(metadata, indent=2) + "\n")
        row = import_oha(result.stdout, metadata)
        expected_errors = {"success": 0, "http-error": 12, "mixed": 8, "transport-error": 12}[case]
        if len(received) != 12 or row["requests"] != 12 or row["errors"] != expected_errors:
            raise ValueError(f"{case}: server observations and normalized result disagree")
        if (row["p99_ms"] is None) != (case == "transport-error"):
            raise ValueError("missing latency must be explicit for all-transport-error run")
        rows.append(row)
        observations.append({"case": case, "server_requests": len(received),
                             "http_200": received.count(200), "http_503": received.count(503),
                             "disconnects": received.count(None), "normalized_errors": row["errors"]})
    (output / "normalized.jsonl").write_text("".join(json.dumps(row) + "\n" for row in rows))
    report = {"scope": "real oha against synthetic loopback handler; NOT nanovm performance",
              "observations": observations, "summary": summarize(rows)}
    (output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--oha", type=Path, required=True)
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True, help="new directory under an existing parent")
    args = parser.parse_args()
    try:
        print(json.dumps(run_fixture(args.oha, args.repo, args.output), indent=2))
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        parser.exit(2, f"local fixture failure: {type(error).__name__}; retain partial output for inspection\n")


if __name__ == "__main__":
    main()
