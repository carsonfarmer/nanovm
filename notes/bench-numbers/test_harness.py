"""No services, compiles or benchmark traffic: synthetic subprocess/telemetry probes."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch, Mock

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("drive", HERE / "drive.py")
drive = importlib.util.module_from_spec(spec)
spec.loader.exec_module(drive)


class HarnessTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.work = Path(self.tmp.name)
        drive.ARGS = SimpleNamespace(oha="oha", duration="6s")
        self.build = dict(name="current", commit="abc", identity="hash", tree=self.work, bin=self.work)

    def oha_result(self, codes, errors=None, status=0, **workload):
        payload = dict(summary=dict(total=1, requestsPerSec=10), statusCodeDistribution=codes,
                       errorDistribution=errors or {})
        proc = Mock(pid=123, stdout=io.BytesIO(json.dumps(payload).encode()))
        ru = SimpleNamespace(ru_utime=0.1, ru_stime=0.1)
        with patch.object(drive.subprocess, "Popen", return_value=proc), \
                patch.object(drive.os, "wait4", return_value=(123, status, ru)):
            return drive.oha("http://unused/", 1, **{"n": 10, **workload})

    def test_duration_commands_drain_ongoing_requests(self):
        timed = self.oha_result({"200": 10}, n=None, z="2s")
        counted = self.oha_result({"200": 10})
        self.assertIn("--wait-ongoing-requests-after-deadline", timed["argv"])
        self.assertNotIn("--wait-ongoing-requests-after-deadline", counted["argv"])

    def test_http_failures_are_invalid(self):
        for code in ("429", "500"):
            with self.subTest(code=code):
                r = self.oha_result({"200": 9, code: 1})
                self.assertFalse(r["valid"])
                self.assertEqual(r["failed"], 1)

    def test_transport_and_exit_failures_are_invalid(self):
        self.assertFalse(self.oha_result({"200": 9}, {"timeout": 1})["valid"])
        self.assertFalse(self.oha_result({"200": 10}, status=256)["valid"])

    def test_short_count_invalid_and_success_valid(self):
        self.assertFalse(self.oha_result({"200": 9})["valid"])
        self.assertTrue(self.oha_result({"200": 10})["valid"])

    def test_inproc_exit_and_missing_json_fail(self):
        for code in (0, 1):
            with self.subTest(code=code), patch.object(drive.subprocess, "run", return_value=
                    SimpleNamespace(returncode=code, stdout="", stderr="probe failure")), \
                    patch.object(drive, "record"):
                with self.assertRaises(RuntimeError):
                    drive.phase_inproc(self.build, 1, self.work)

    def test_relative_paths_resolved_and_existing_roots_refused(self):
        args = SimpleNamespace(work="fresh-work", out="fresh-out")
        with contextlib.chdir(self.work):
            drive.prepare_paths(args)
        self.assertTrue(Path(args.work).is_absolute())
        with self.assertRaises(FileExistsError):
            drive.prepare_paths(args)

    def inproc_json(self):
        run = dict(requests=10, failed=0)
        return dict(cold=dict(run=run), neighbours=dict(baseline=run, under_attack=run),
                    **{k: run for k in ("warm_get", "warm_serial", "warm_incr", "warm_incr_serial", "recover", "churn")})

    def test_inproc_real_subprocess_cwd_and_failure_json(self):
        # A tiny executable writes fixture JSON, not a nanovm benchmark.
        stub = self.work / "bench"
        payload = self.inproc_json()
        stub.write_text(f"#!{sys.executable}\n" + """import json, os, sys
from pathlib import Path
assert Path(os.environ['NANOVM_BENCH_DATA']).is_absolute()
if '--json' in sys.argv:
    p = Path(sys.argv[sys.argv.index('--json') + 1])
    assert p.is_absolute()
    p.write_text(json.dumps(""" + repr(payload) + """))
print('fixture run')
""")
        stub.chmod(0o755)
        (self.work / "tree").mkdir()
        self.build["tree"] = self.work / "tree"
        (self.work / "run").mkdir()
        with contextlib.chdir(self.work), patch.object(drive, "record") as record:
            drive.phase_inproc(self.build, 1, Path("run"))
        self.assertTrue(all(call.args[0]["valid"] for call in record.call_args_list))
        stub.write_text(stub.read_text().replace("print('fixture run')",
                        "print('FAILED 1' if '--writes' in sys.argv else 'fixture run')"))
        with contextlib.chdir(self.work), patch.object(drive, "record"):
            with self.assertRaises(RuntimeError):
                drive.phase_inproc(self.build, 2, Path("run"))
        payload["cold"]["run"] = dict(requests=10, failed=1)
        self.assertFalse(drive.inproc_valid(payload))

    def test_run_case_records_failure_and_rejects_it(self):
        cpu = Mock()
        result = self.oha_result({"200": 9, "500": 1})
        with patch.object(drive, "oha", return_value=result), patch.object(drive, "record") as record, \
                patch.object(drive.Cpu, "delta", return_value={}):
            with self.assertRaises(RuntimeError):
                drive.run_case(self.build, 1, None, cpu, "direct", "get", "http://unused", 1, warm=False)
            self.assertFalse(record.call_args.args[0]["valid"])

    def test_throughput_matrix_separates_hot_and_spread_concurrency(self):
        rows = []

        def capture(b, rnd, st, cpu, phase, case, url, conc, **kwargs):
            rows.append((case, conc))

        with patch.object(drive, "run_case", side_effect=capture):
            drive.phase_throughput(self.build, 1, None, {"counter": "ref"}, Mock())
        for case in ("get_hot", "incr_hot"):
            self.assertEqual([conc for name, conc in rows if name == case], [1, 16, 64])
        self.assertIn(("get_spread", 256), rows)
        self.assertIn(("incr_spread", 256), rows)

    def test_v1_config_retention_and_clients(self):
        import tomllib
        node = tomllib.loads(drive.node_toml(self.build, self.work))
        proxy = tomllib.loads(drive.proxy_toml(self.build))
        self.assertGreaterEqual(node["limits"]["idle_timeout_ms"], 3600000)
        # Inherit the current connection-only defaults; no rate-limit overrides.
        self.assertNotIn("clients", node)
        self.assertNotIn("clients", proxy)
        self.assertEqual(proxy["nodes"], [drive.NODE])

    def test_density_observed_live_count_and_target_pid(self):
        snapshot = dict(collected_at_ms=2000, workers={"app": dict(up=True, pid=77)},
                        apps={"app": dict(live_objects=2001, stores_live=2001)})
        (self.work / "target").mkdir()
        (self.work / "target/demo-telemetry.json").write_text(json.dumps(snapshot))
        with patch.object(drive, "rss_kb", return_value=4000), patch.object(drive.time, "sleep"):
            row = drive.density_snapshot(self.work, "app", 2001, 1000, pid=77, timeout=0.01)
            self.assertEqual(row["pid"], 77)
            self.assertEqual(row["live"], 2001)
            for change in (dict(expected=2002), dict(pid=78), dict(after_ms=3000)):
                kw = dict(expected=2001, after_ms=1000, pid=77, timeout=0.001)
                kw.update(change)
                with self.assertRaises(RuntimeError):
                    drive.density_snapshot(self.work, "app", **kw)

    def test_density_dedicated_stack_verified_increment_and_cleanup(self):
        guest = self.work / "guests/counter"
        guest.mkdir(parents=True)
        (guest / "nanovm.json").write_text(json.dumps(dict(limits=dict(idle_timeout_ms=60000))))
        rows = [dict(live=n, worker_kb=1000 + (n - 1) * 100, pid=77) for n in (1, 2001, 4001, 6001)]
        replies = [SimpleNamespace(stdout="registry/density:1.0.0@sha256:abc"),
                   SimpleNamespace(stdout=json.dumps([dict(app="opaque", pid=77)]))]
        with patch.object(drive.subprocess, "run", side_effect=replies) as run, \
                patch.object(drive, "Proc") as proc, patch.object(drive, "Stack") as stack, \
                patch.object(drive, "wait_port"), patch.object(drive, "oha", return_value=dict(valid=True)) as load, \
                patch.object(drive, "density_snapshot", side_effect=rows) as snap, \
                patch.object(drive, "record") as record, patch.object(drive, "log"):
            drive.phase_density(self.build, 1, self.work, {})
        rec = record.call_args.args[0]
        self.assertEqual(rec["live_delta"], 6000)
        self.assertEqual(rec["kb_per_object"], 100)
        self.assertEqual([c.args[2] for c in snap.call_args_list], [1, 2001, 4001, 6001])
        self.assertTrue(all(c.kwargs["pid"] == 77 for c in snap.call_args_list))
        self.assertEqual(stack.call_args.args[2], self.work / "density/data")
        manifest = json.loads((self.work / "density/density.json").read_text())
        self.assertEqual(manifest["limits"]["idle_timeout_ms"], drive.RETENTION_MS)
        self.assertEqual(load.call_args.kwargs["headers"][0], "nanovm-app: registry/density@sha256:abc")
        self.assertIn("density:1.0.0", run.call_args_list[0].args[0][3])
        stack.return_value.stop.assert_called_once()
        proc.return_value.stop.assert_called_once()

    def test_density_refuses_unsuccessful_creation(self):
        guest = self.work / "guests/counter"
        guest.mkdir(parents=True)
        (guest / "nanovm.json").write_text('{"limits": {}}')
        with patch.object(drive.subprocess, "run", return_value=
                SimpleNamespace(stdout="registry/density:1.0.0@sha256:abc")), \
                patch.object(drive, "Proc") as proc, patch.object(drive, "Stack") as stack, \
                patch.object(drive, "wait_port"), patch.object(drive, "oha", return_value=dict(valid=False)), \
                patch.object(drive, "density_snapshot") as snapshot, patch.object(drive, "record") as record:
            with self.assertRaises(RuntimeError):
                drive.phase_density(self.build, 1, self.work, {})
            snapshot.assert_not_called()
            record.assert_not_called()
            stack.return_value.stop.assert_called_once()
            proc.return_value.stop.assert_called_once()

    def summary(self, rows):
        src = self.work / "raw.jsonl"
        src.write_text("".join(json.dumps(r) + "\n" for r in rows))
        return subprocess.run([sys.executable, str(HERE / "summarize.py"), str(src)],
                              capture_output=True, text=True)

    def row(self, **kw):
        return dict(build="current", commit="abc", identity="hash", round=1, phase="fixed_rate",
                    case="get@50%", conc=64, q=100, valid=True, rps=100, **kw)

    def test_summary_duplicate_rejected(self):
        r = self.row()
        self.assertNotEqual(self.summary([r, r]).returncode, 0)

    def test_summary_commit_binary_and_rate_mismatch_rejected(self):
        for field, value in (("commit", "def"), ("identity", "other"), ("q", 200)):
            a, b = self.row(), self.row()
            b.update(round=2, **{field: value})
            with self.subTest(field=field):
                self.assertNotEqual(self.summary([a, b]).returncode, 0)

    def test_summary_invalid_rejected_and_clean_rounds_accepted(self):
        a, b = self.row(), self.row()
        b.update(round=2)
        self.assertEqual(self.summary([a, b]).returncode, 0)
        b.update(valid=False, codes={"500": 10})
        self.assertNotEqual(self.summary([a, b]).returncode, 0)

    def test_summary_failures_not_hidden_by_valid_flag(self):
        a = self.row()
        a.update(codes={"200": 9, "503": 1})
        self.assertNotEqual(self.summary([a]).returncode, 0)

    def test_summary_cannot_exempt_failed_responses(self):
        a = self.row()
        a.update(workload="rejection", codes={"200": 9, "429": 100}, p99_ms=0.1)
        result = self.summary([a])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("failed responses", result.stderr)

    def test_only_phases_validated_without_workloads(self):
        self.assertEqual(drive.parse_phases("direct,density"), {"direct", "density"})
        with self.assertRaises(drive.argparse.ArgumentTypeError):
            drive.parse_phases("unknown")

    def test_cli_help_without_workloads(self):
        result = subprocess.run([sys.executable, str(HERE / "drive.py"), "--help"],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--rates", result.stdout)


if __name__ == "__main__":
    unittest.main()
