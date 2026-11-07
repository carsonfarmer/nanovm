import unittest

from load_exercise import exercise


class LoadTests(unittest.TestCase):
    def test_cap_rate_skew_and_error_oracle(self):
        plan = dict(kind='popularity', basis='synthetic', distribution='hotset', seed=7,
                    objects=4, requests=12, hot_objects=1, hot_share=1)
        result = exercise(plan, concurrency=3, rate=100, service_ms=80, fail_every=3)
        self.assertEqual(result['requests'], 12)
        self.assertEqual(result['errors'], 4)
        self.assertEqual(result['observed_object_counts'], [12, 0, 0, 0])
        self.assertLessEqual(result['server_peak_active'], 3)
        self.assertGreater(result['server_peak_active'], 1)
        self.assertGreaterEqual(result['minimum_start_spacing_s'], 0.01 - 1e-6)
        plan['requests'] = 1
        self.assertIsNone(exercise(plan, 1, 100, 0)['minimum_start_spacing_s'])


if __name__ == '__main__':
    unittest.main()
