#!/usr/bin/env python3
"""Bounded local Collector redaction/enrichment check with synthetic OTLP JSON."""

import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

from benchmark_import import pinned_tool

HERE = Path(__file__).resolve().parent
COLLECTOR_VERSION = "otelcol-contrib version 0.164.0"
CANARY = "FAKE_SECRET_CANARY_COLLECTOR_9281"


def payload():
    def attr(key, value):
        return {"key": key, "value": {"stringValue": value}}

    spans = []
    for name, span_id, parent, kind in [("supervisor.request", "1111111111111111", "", 2),
                                         ("request", "2222222222222222", "1111111111111111", 2)]:
        spans.append({"traceId": "0123456789abcdef0123456789abcdef", "spanId": span_id,
                      "parentSpanId": parent, "name": name, "kind": kind,
                      "startTimeUnixNano": "1794000000000000000", "endTimeUnixNano": "1794000000001000000",
                      "attributes": [attr("demo.secret", CANARY), attr("nanovm.app", "fixture-app"),
                                     attr("example.operation_id", "fixture-op-1")]})
    return {"resourceSpans": [{"resource": {"attributes": [attr("service.name", "nice-fixture"),
                                                             attr("nanovm.app", "fixture-app"),
                                                             attr("demo.secret", CANARY)]},
                                "scopeSpans": [{"scope": {"name": "nice-fixture"}, "spans": spans}]}]}


def verify_export(documents):
    resources = [r for doc in documents for r in doc.get("resourceSpans", [])]
    spans = [s for r in resources for scope in r.get("scopeSpans", []) for s in scope.get("spans", [])]
    if len(spans) != 2:
        raise ValueError("expected exactly two exported synthetic spans")
    if CANARY in json.dumps(documents):
        raise ValueError("secret canary survived redaction")
    for resource in resources:
        attrs = {a["key"]: a["value"] for a in resource["resource"]["attributes"]}
        if attrs.get("nanovm.app") != {"stringValue": "fixture-app"}:
            raise ValueError("resource identity changed")
        if attrs.get("deployment.environment.name") != {"stringValue": "nice-to-haves-fixture"}:
            raise ValueError("environment enrichment missing")
    by_name = {s["name"]: s for s in spans}
    if set(by_name) != {"request", "supervisor.request"}:
        raise ValueError("span names changed or duplicated")
    for span in spans:
        attrs = {a["key"]: a["value"] for a in span["attributes"]}
        if attrs.get("nanovm.app") != {"stringValue": "fixture-app"} or attrs.get("example.operation_id") != {"stringValue": "fixture-op-1"}:
            raise ValueError("allowed span attributes changed")
        if span["traceId"] != "0123456789abcdef0123456789abcdef":
            raise ValueError("trace identity changed")
    if by_name["request"].get("parentSpanId") != by_name["supervisor.request"]["spanId"]:
        raise ValueError("trace parentage changed")
    return {"exported_spans": 2, "canary_removed": True, "identity_and_parentage_preserved": True,
            "environment": "nice-to-haves-fixture"}


def run_fixture(collector, config=HERE / "fixtures/collector.yaml"):
    collector = Path(collector).resolve(strict=True)
    identity = pinned_tool(collector, COLLECTOR_VERSION)
    with tempfile.TemporaryDirectory(prefix="nice-collector-") as directory:
        exported = Path(directory) / "traces.json"
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            port = reservation.getsockname()[1]
        env = {"PATH": os.defpath, "NICE_OTLP_PORT": str(port), "NICE_EXPORT_FILE": str(exported)}
        command = [str(collector), "--config", str(Path(config).resolve())]
        subprocess.run([str(collector), "validate", *command[1:]], env=env,
                       check=True, capture_output=True, timeout=10)
        # Short-lived test process only; finally always terminates and reaps it.
        with (Path(directory) / "collector.log").open("wb") as log:
            process = subprocess.Popen(command, env=env, stdout=log, stderr=log)
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

            def post(body):
                request = urllib.request.Request(f"http://127.0.0.1:{port}/v1/traces",
                                                 data=json.dumps(body).encode(),
                                                 headers={"Content-Type": "application/json"})
                with opener.open(request, timeout=1) as response:
                    if response.status != 200:
                        raise ValueError("OTLP request failed")
                    reply = json.loads(response.read(65536))
                    if reply.get("partialSuccess", {}).get("rejectedSpans", "0") not in (0, "0"):
                        raise ValueError("Collector rejected synthetic spans")

            try:
                deadline = time.monotonic() + 10
                while True:
                    if process.poll() is not None:
                        raise ValueError("Collector exited before readiness")
                    try:
                        post({"resourceSpans": []})
                        break
                    except urllib.error.URLError:
                        if time.monotonic() >= deadline:
                            raise TimeoutError("Collector readiness deadline")
                        time.sleep(0.05)
                post(payload())  # Never retry the actual span payload.
            finally:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
            if process.returncode != 0:
                raise ValueError("Collector did not shut down cleanly")
        documents = [json.loads(line) for line in exported.read_text().splitlines() if line.strip()]
        return {"tool": identity, **verify_export(documents),
                "scope": "synthetic traces through loopback OTLP JSON to disposable file exporter; not a production privacy guarantee"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--collector", type=Path, required=True)
    args = parser.parse_args()
    try:
        print(json.dumps(run_fixture(args.collector), indent=2))
    except (OSError, ValueError, TypeError, KeyError, subprocess.SubprocessError) as error:
        parser.exit(2, f"Collector fixture failed: {type(error).__name__}\n")


if __name__ == "__main__":
    main()
