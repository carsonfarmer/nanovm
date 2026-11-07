import copy
import json
from pathlib import Path
import unittest

from trace_view import normalize, render


class TraceTests(unittest.TestCase):
    def test_intervals_missing_parent_and_allowlist(self):
        data = json.loads(Path(__file__).with_name('fixtures').joinpath('traces.json').read_text())
        result = normalize(data)
        self.assertEqual([s['duration_ms'] for s in result[0]['spans']], [12, 9, 3])
        self.assertEqual([s['start_ms'] for s in result[0]['spans']], [0, 1, 4])
        self.assertEqual(result[0]['spans'][2]['parent'], 'Span 2')
        self.assertTrue(result[1]['spans'][0]['missing_parent'])
        html = render(data)
        self.assertNotIn('FAKE_SECRET_CANARY', html)
        self.assertNotIn('1794000000000000000', html)
        self.assertNotIn('11111111111111111111111111111111', html)
        spans = data['resourceSpans'][0]['scopeSpans'][0]['spans']
        for mutation in ['duplicate', 'cycle', 'backward', 'invalid-id']:
            changed = copy.deepcopy(data)
            items = changed['resourceSpans'][0]['scopeSpans'][0]['spans']
            if mutation == 'duplicate': items.append(copy.deepcopy(items[0]))
            if mutation == 'cycle': items[0]['parentSpanId'] = items[2]['spanId']
            if mutation == 'backward': items[0]['endTimeUnixNano'] = '1'
            if mutation == 'invalid-id': items[0]['spanId'] = ''
            with self.assertRaises(ValueError, msg=mutation): normalize(changed)
        spans[1]['endTimeUnixNano'] = '1794000000020000000'
        self.assertTrue(normalize(data)[0]['spans'][1]['outside_parent'])
        self.assertEqual(normalize({}), [])
        for invalid in ['0' * 16, 'z' * 16, None]:
            changed = copy.deepcopy(data)
            changed['resourceSpans'][0]['scopeSpans'][0]['spans'][0]['spanId'] = invalid
            with self.assertRaises(ValueError): normalize(changed)
        spans[0]['endTimeUnixNano'] = spans[0]['startTimeUnixNano']
        self.assertEqual(normalize(data)[0]['spans'][0]['duration_ms'], 0)
        spans[0]['startTimeUnixNano'] = str(2**64)
        with self.assertRaises(ValueError): normalize(data)


if __name__ == '__main__':
    unittest.main()
