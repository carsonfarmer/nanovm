"""Opt-in integration check against the disposable batch-9 Prometheus fixture."""

import json
import os
from pathlib import Path
import unittest
from urllib.parse import urlencode
from urllib.request import ProxyHandler, build_opener


@unittest.skipUnless(os.environ.get('NICE_SYNTHETIC_PROMETHEUS') == '1', 'start batch-9 local Prometheus fixture')
class DashboardTests(unittest.TestCase):
    def test_real_queries_and_double_count_control(self):
        dashboard = json.loads(Path(__file__).with_name('fixtures').joinpath('grafana/content/dashboard.json').read_text())
        opener = build_opener(ProxyHandler({}))

        def query(expression):
            url = 'http://127.0.0.1:8791/api/v1/query?' + urlencode({'query': expression})
            with opener.open(url, timeout=5) as response:
                result = json.load(response)
            self.assertEqual(result['status'], 'success')
            return result['data']['result']

        for panel, expected in zip(dashboard['panels'], [100, .2, .35, {'error': 20, 'ok': 80}, 5]):
            values = query(panel['targets'][0]['expr'])
            if isinstance(expected, dict):
                observed = {v['metric']['outcome']: float(v['value'][1]) for v in values}
                self.assertEqual(observed.keys(), expected.keys())
                for key, value in expected.items():
                    self.assertAlmostEqual(observed[key], value, delta=.05)
            else:
                self.assertEqual(len(values), 1)
                self.assertAlmostEqual(float(values[0]['value'][1]), expected, delta=.05)
        # Dropping the layer selector must demonstrably double-count, not pass unnoticed.
        self.assertAlmostEqual(float(query('sum(rate(nice_requests_total[30s]))')[0]['value'][1]), 200, delta=.1)


if __name__ == '__main__':
    unittest.main()
