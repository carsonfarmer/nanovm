from copy import deepcopy
import json
from pathlib import Path
import unittest

from analysis_tools import merge_histograms, topology, transcript

HERE = Path(__file__).resolve().parent


class AnalysisContracts(unittest.TestCase):
    def test_transcript_stable_and_omits_private_labels(self):
        rows = [json.loads(line) for line in (HERE / "fixtures/benchmarks.jsonl").read_text().splitlines()]
        for row in rows:
            row["environment"] = '<script>SECRET_CANARY|\ncredential</script>'
        text = transcript(rows)
        self.assertEqual(text, transcript(list(reversed(rows))))
        self.assertNotIn("SECRET_CANARY", text)
        self.assertNotIn("<script>", text)
        self.assertIn("| synthetic | http-node | 2 | 10000 | 90 | 2477.5 | 8–12 | 0 | false |", text)

    def test_merge_disjoint_counts_and_rank_intervals(self):
        rows = json.loads((HERE / "fixtures/histograms.json").read_text())
        result = merge_histograms(rows)
        self.assertEqual(result["counts"], [2, 3, 3, 2])
        self.assertEqual(result["samples"], 10)
        self.assertEqual(result["quantile_intervals"]["p50"], {"lower": 1, "lower_inclusive": False, "upper_inclusive": 5})
        self.assertIsNone(result["quantile_intervals"]["p99"]["upper_inclusive"])
        self.assertEqual(result, merge_histograms(rows[::-1]))

    def test_histograms_reject_mixed_inputs_and_keep_empty_unknown(self):
        rows = json.loads((HERE / "fixtures/histograms.json").read_text())
        for key, value in [("unit", "s"), ("bounds", [1, 6, 20]), ("scope", "other"),
                           ("provenance", "measured"), ("run", "a"), ("counts", [True, 2, 3, 1])]:
            changed = deepcopy(rows)
            changed[1][key] = value
            with self.subTest(key=key), self.assertRaises(ValueError):
                merge_histograms(changed)
        rows[0]["counts"] = [0, 0, 0, 0]
        self.assertEqual(merge_histograms(rows[:1])["quantile_intervals"], {"p50": None, "p95": None, "p99": None})

    def test_topology_uses_socket_plus_core_and_checks_smt_overlap(self):
        data = {"cpus": [{"cpu": 0, "core": 0, "socket": 0, "online": True},
                         {"cpu": 1, "core": 0, "socket": 0, "online": True},
                         {"cpu": "2", "core": "0", "socket": "1", "online": "yes"},
                         {"cpu": 3, "core": 1, "socket": 1, "online": False}]}
        self.assertFalse(topology(data, [0, 1, 2, 3], [0], [1])["cpu_isolation_established"])
        result = topology(data, [0, 1, 2, 3], [0], [2])
        self.assertTrue(result["cpu_isolation_established"])
        self.assertEqual(result["eligible_logical_cpus"], [0, 1, 2])
        self.assertEqual(len(result["physical_core_groups"]), 2)
        self.assertFalse(topology(data, [0, 1, 2])["cpu_isolation_established"])
        with self.assertRaises(ValueError):
            topology(data, [0], [0], [2])
        with self.assertRaises(ValueError):
            topology({"cpus": [{"cpu": 0, "core": "-", "socket": 0, "online": True}]}, [0])


if __name__ == "__main__":
    unittest.main()
