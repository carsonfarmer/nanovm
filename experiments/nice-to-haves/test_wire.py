import shutil
import subprocess
from pathlib import Path
import unittest

from wire_fixture import exercise


@unittest.skipUnless(shutil.which('node'), 'Node.js required for real JS/Python wire fixture')
class WireTests(unittest.TestCase):
    def test_exact_bytes_lost_replies_and_explicit_retry_contract(self):
        result = exercise()
        self.assertEqual(result['requests'], 5)
        self.assertTrue(all(result['checks'].values()))

    def test_javascript_refuses_external_target_before_reading_body(self):
        result = subprocess.run(['node', str(Path(__file__).with_name('operation_client.mjs')),
                                 'http://example.com/never', 'operation', '/missing'],
                                capture_output=True, timeout=5)
        self.assertEqual(result.returncode, 2)
        self.assertEqual(result.stdout, b'')


if __name__ == '__main__':
    unittest.main()
