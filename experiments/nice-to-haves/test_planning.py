import unittest

from planning import calculate


def run(kind, **fields):
    return calculate(dict(basis='synthetic', kind=kind, **fields))['result']


class PlanningTests(unittest.TestCase):
    def test_amplification_rounds_each_file(self):
        result = run('amplification', block_bytes=4096, metadata_bytes_per_file=128,
                     file_bytes=[0, 1, 4096, 4097])
        self.assertEqual(result['logical_bytes'], 8194)
        self.assertEqual(result['data_blocks_bytes'], 16384)
        self.assertEqual(result['metadata_bytes'], 512)
        self.assertEqual(result['amplification'], 16896 / 8194)
        self.assertIsNone(run('amplification', block_bytes=1, metadata_bytes_per_file=4,
                              file_bytes=[0])['amplification'])

    def test_cardinality_product_budget_and_overflow(self):
        args = dict(metric_families=3, series_per_labelset=7, label_value_counts=[2, 5])
        self.assertFalse(run('cardinality', series_budget=210, **args)['over_budget'])
        self.assertTrue(run('cardinality', series_budget=209, **args)['over_budget'])
        self.assertEqual(run('cardinality', series_budget=210, **args)['series_upper_bound'], 210)
        args['label_value_counts'] = [10**12, 10**12]
        result = run('cardinality', series_budget=0, **args)
        self.assertEqual(result['series_upper_bound'], 21 * 10**24)
        self.assertTrue(result['exceeds_signed_64bit'])

    def test_cost_keeps_alternatives_separate(self):
        self.assertEqual(run('cost', hours=4, nodes=3, watts_per_node=250,
                             cost_per_kwh=0.2, cloud_cost_per_node_hour=0.5),
                         {'kwh': 3, 'electricity_cost': 0.6000000000000001, 'cloud_cost': 6})

    def test_queue_idle_simultaneous_and_zero_service(self):
        jobs = [dict(arrival_ms=a, service_ms=s) for a, s in [(2, 4), (3, 2), (3, 0), (12, 1)]]
        self.assertEqual(run('queue', jobs=jobs),
                         {'wait_ms': [0, 3, 5, 0], 'finish_ms': [6, 8, 8, 13], 'mean_wait_ms': 2})
        self.assertIsNone(run('queue', jobs=[])['mean_wait_ms'])
        with self.assertRaises(ValueError):
            run('queue', jobs=list(reversed(jobs)))

    def test_balanced_order_seed_replay(self):
        result = run('order', pairs=20, seed=17)
        self.assertEqual(result, run('order', pairs=20, seed=17))
        self.assertNotEqual(result['pairs'], run('order', pairs=20, seed=18)['pairs'])
        self.assertEqual(result['pairs'].count('AB'), 10)
        self.assertEqual(result['pairs'].count('BA'), 10)
        with self.assertRaises(ValueError):
            run('order', pairs=3, seed=0)

    def test_popularity_exact_counts_extremes_and_replay(self):
        for distribution, fields in [('uniform', {}), ('zipf', {'exponent': 1.5}),
                                      ('hotset', {'hot_objects': 2, 'hot_share': 1})]:
            args = dict(distribution=distribution, objects=5, requests=1000, seed=23, **fields)
            result = run('popularity', **args)
            self.assertEqual(result, run('popularity', **args))
            self.assertEqual(sum(result['counts']), 1000)
            self.assertEqual(result['counts'], [result['sequence'].count(i) for i in range(5)])
            if distribution == 'hotset':
                self.assertEqual(result['counts'][2:], [0, 0, 0])
            if distribution == 'zipf':
                self.assertGreater(result['counts'][0], 3 * result['counts'][-1])
        self.assertEqual(run('popularity', distribution='uniform', objects=1, requests=3,
                             seed=0)['sequence'], [0, 0, 0])
        self.assertEqual(run('popularity', distribution='hotset', objects=2, requests=3,
                             seed=0, hot_objects=1, hot_share=0)['sequence'], [1, 1, 1])

    def test_rejects_measured_claim_and_invalid_bounds(self):
        with self.assertRaises(ValueError):
            calculate({'basis': 'measured', 'kind': 'cost'})
        for value in [-1, True, float('nan'), float('inf')]:
            with self.assertRaises(ValueError):
                run('cost', hours=value, nodes=1, watts_per_node=1,
                    cost_per_kwh=1, cloud_cost_per_node_hour=1)
        with self.assertRaises(ValueError):
            run('popularity', objects=1, requests=1, seed=1, distribution='hotset',
                hot_objects=1, hot_share=1)


if __name__ == '__main__':
    unittest.main()
