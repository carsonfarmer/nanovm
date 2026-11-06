"""Offline regression fixtures plus opt-in real pinned-tool contract checks."""

from copy import deepcopy
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from benchmark_import import digest, import_oha, manifest, pinned_tool
from collector_fixture import run_fixture as run_collector
from local_load_fixture import run_fixture
from toolkit import summarize
from wit_inventory import inventory

HERE = Path(__file__).resolve().parent
OHA = os.environ.get("NICE_OHA")
WASM_TOOLS = os.environ.get("NICE_WASM_TOOLS")
COLLECTOR = os.environ.get("NICE_COLLECTOR")


def bound_metadata(raw, case):
    scenario = json.loads((HERE / "fixtures/scenario.json").read_text())
    scenario.update(run=case, workload=case, variant="python-loopback-fixture",
                    environment="archived-loopback-fixture", provenance="measured")
    return {"schema": "nanovm-nice-benchmark-v1", "tool": {"version": "oha 1.16.0"},
            "raw": {"sha256": digest(raw), "bytes": len(raw)}, "scenario": scenario}


class ImportContracts(unittest.TestCase):
    def test_archived_real_oha_outputs_count_http_and_transport_errors(self):
        for case, errors in [("success", 0), ("http-error", 12), ("mixed", 8), ("transport-error", 12)]:
            with self.subTest(case=case):
                raw = (HERE / "fixtures/oha" / f"{case}.json").read_bytes()
                row = import_oha(raw, bound_metadata(raw, case))
                self.assertEqual((row["requests"], row["errors"]), (12, errors))
                self.assertEqual(row["p99_ms"] is None, case == "transport-error")

    def test_units_and_explicit_status_oracle_not_oha_success_rate(self):
        # Asymmetric synthetic values make seconds-vs-ms and 2xx-only mistakes visible.
        raw = json.dumps({"summary": {"total": 4, "successRate": 1},
                          "statusCodeDistribution": {"200": 3, "201": 2, "404": 4},
                          "errorDistribution": {"transport": 3}, "latencyPercentiles": {"p99": 0.125}}).encode()
        metadata = bound_metadata(raw, "units")
        metadata["scenario"]["provenance"] = "synthetic"
        metadata["scenario"]["accepted_statuses"] = [200, 404]
        row = import_oha(raw, metadata)
        self.assertEqual(row["errors"], 5)
        self.assertEqual(row["p99_ms"], 125)
        self.assertEqual(summarize([row])["groups"][0]["successful_rps"], 1.75)

    def test_all_failed_runs_survive_summary_without_invented_latency(self):
        raw = (HERE / "fixtures/oha/transport-error.json").read_bytes()
        row = import_oha(raw, bound_metadata(raw, "failed"))
        group = summarize([row])["groups"][0]
        self.assertEqual(group["requests"], 12)
        self.assertEqual(group["successful_rps"], 0)
        self.assertIsNone(group["run_p99_ms_range"])
        self.assertEqual(group["runs_without_p99"], 1)
        other = dict(row, run="second", p99_ms=50, errors=0)
        combined = summarize([row, other])["groups"][0]
        self.assertEqual(combined["requests"], 24)
        self.assertEqual(combined["errors"], 12)
        self.assertEqual(combined["run_p99_ms_range"], [50, 50])
        self.assertEqual(combined["runs_without_p99"], 1)

    def test_wrong_hash_version_counts_and_nonfinite_latency_fail(self):
        raw = (HERE / "fixtures/oha/success.json").read_bytes()
        metadata = bound_metadata(raw, "bad")
        for mutate in (lambda m: m["raw"].update(sha256="wrong"),
                       lambda m: m["tool"].update(version="oha 1.17.0"),
                       lambda m: m["scenario"].update(planned_requests=13)):
            changed = deepcopy(metadata)
            mutate(changed)
            with self.assertRaises(ValueError):
                import_oha(raw, changed)
        for field, value in [("statusCodeDistribution", {"200": True}),
                             ("latencyPercentiles", {"p99": float("nan")})]:
            changed = json.loads(raw)
            changed[field] = value
            data = json.dumps(changed).encode()
            with self.assertRaises(ValueError):
                import_oha(data, bound_metadata(data, "bad"))

    def test_import_cli_reports_safe_failure(self):
        secret = "FAKE_SECRET_CANARY_9281"
        with tempfile.TemporaryDirectory() as directory:
            raw = Path(directory) / "raw.json"
            raw.write_text(secret)
            meta = Path(directory) / "manifest.json"
            meta.write_text(json.dumps(bound_metadata(raw.read_bytes(), "bad")))
            result = subprocess.run([sys.executable, "-B", str(HERE / "benchmark_import.py"), "import-oha",
                                     "--raw", str(raw), "--manifest", str(meta)], capture_output=True, text=True)
            self.assertEqual(result.returncode, 2)
            self.assertNotIn(secret, result.stdout + result.stderr)
            self.assertNotIn("Traceback", result.stderr)


@unittest.skipUnless(OHA, "set NICE_OHA to pinned oha binary for real tool checks")
class OhaContracts(unittest.TestCase):
    def test_metadata_does_not_emit_environment_paths_contents_or_remote_credentials(self):
        secret = "FAKE_SECRET_CANARY_9281"
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)

            def git(*args):
                subprocess.run(["git", "-C", str(repo), *args], check=True, capture_output=True)

            git("init", "-q")
            tracked = repo / f"{secret}.txt"
            tracked.write_text("initial\n")
            git("add", ".")
            git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "fixture")
            tracked.write_text(secret)
            git("remote", "add", "origin", f"https://user:{secret}@example.invalid/repo")
            (repo / "untracked").write_text(secret)
            raw = HERE / "fixtures/oha/success.json"
            scenario = bound_metadata(raw.read_bytes(), "privacy")["scenario"]
            with patch.dict(os.environ, {"NICE_TEST_SECRET": secret}):
                result = manifest(repo, raw, scenario, OHA, [("component", tracked)])
            self.assertNotIn(secret, json.dumps(result))
            self.assertTrue(result["source"]["tracked_dirty"])
            self.assertEqual(result["source"]["untracked_file_count"], 1)
            self.assertEqual(result["artifacts"]["component"]["sha256"], digest(secret.encode()))
            self.assertEqual(result["tool"]["version"], "oha 1.16.0")
            self.assertEqual(import_oha(raw.read_bytes(), result)["errors"], 0)

    def test_local_fixture_matches_real_observations_and_refuses_overwrite(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "results"
            report = run_fixture(OHA, HERE, output)
            self.assertEqual([x["server_requests"] for x in report["observations"]], [12] * 4)
            self.assertEqual([x["normalized_errors"] for x in report["observations"]], [0, 12, 8, 12])
            self.assertEqual(report["observations"][2]["disconnects"], 4)
            before = (output / "report.json").read_bytes()
            with self.assertRaises(FileExistsError):
                run_fixture(OHA, HERE, output)
            self.assertEqual((output / "report.json").read_bytes(), before)


@unittest.skipUnless(WASM_TOOLS, "set NICE_WASM_TOOLS to pinned wasm-tools binary")
class WitContracts(unittest.TestCase):
    def test_external_functions_types_and_interfaces_survive_inventory(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "fixture.wasm"
            subprocess.run([WASM_TOOLS, "parse", str(HERE / "fixtures/inventory.wat"), "-o", str(binary)],
                           check=True, capture_output=True)
            result = inventory(binary, WASM_TOOLS)
            world = result["worlds"][0]
            self.assertEqual(result["component"]["sha256"], digest(binary.read_bytes()))
            self.assertEqual({x["kind"] for x in world["imports"]}, {"function", "type", "interface"})
            for direction in ("imports", "exports"):
                api = next(x for x in world[direction] if x["kind"] == "interface")
                self.assertEqual((api["package"], api["name"], api["functions"]),
                                 ("example:fixture@1.0.0", "api", ["ping"]))
            self.assertEqual(result["validation_features"], "all")

    def test_core_malformed_and_wrong_version_fail(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "bad.wasm"
            for data in (b"\0asm\x01\0\0\0", b"\0asm\x0d\0\x01\0\x01\x01\xff"):
                binary.write_bytes(data)
                with self.assertRaises((ValueError, subprocess.CalledProcessError)):
                    inventory(binary, WASM_TOOLS)
            with self.assertRaises(ValueError):
                pinned_tool(WASM_TOOLS, "wasm-tools 0.0.0")


@unittest.skipUnless(COLLECTOR, "set NICE_COLLECTOR to pinned Collector binary")
class CollectorContracts(unittest.TestCase):
    def test_real_collector_redacts_enriches_and_preserves_trace_identity(self):
        result = run_collector(COLLECTOR)
        self.assertEqual(result["exported_spans"], 2)
        self.assertTrue(result["canary_removed"])
        self.assertTrue(result["identity_and_parentage_preserved"])

    def test_disabled_redaction_and_wrong_enrichment_fail_the_same_oracle(self):
        source = (HERE / "fixtures/collector.yaml").read_text()
        with tempfile.TemporaryDirectory() as directory:
            config = Path(directory) / "mutated.yaml"
            for old, new, message in [("key: demo.secret", "key: irrelevant", "canary"),
                                      ("value: nice-to-haves-fixture", "value: wrong", "enrichment")]:
                with self.subTest(mutation=old):
                    config.write_text(source.replace(old, new))
                    with self.assertRaisesRegex(ValueError, message):
                        run_collector(COLLECTOR, config)


if __name__ == "__main__":
    unittest.main()
