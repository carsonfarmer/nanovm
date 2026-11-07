from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from config_lint import lint


class ConfigTests(unittest.TestCase):
    def test_cli_refuses_unreviewed_source(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / 'source.rs'
            source.write_text('different source')
            config = Path(directory) / 'config.toml'
            config.write_text('data_dir="secret-canary"')
            result = subprocess.run([sys.executable, '-B', str(Path(__file__).with_name('config_lint.py')),
                                     'node', str(config), '--source', str(source)], capture_output=True)
            self.assertEqual(result.returncode, 2)
            self.assertNotIn(b'secret-canary', result.stdout + result.stderr)

    def test_nested_types_unknown_keys_and_zero_semantics(self):
        self.assertEqual(lint('[worker]\ncpus=0\npids=0\n[limits]\ncpu_ms=0\negress=[]', 'node')['errors'], [])
        for text in ['[worker]\ncpu=2', '[limits]\ncpu_ms=true', 'jail_uid_base=4294967296',
                     '[limits]\negress=[3]', 'max_apps=-1', 'limits="bad"']:
            self.assertTrue(lint(text, 'node')['errors'], text)
        result = lint('data_dir="secret-canary"\n[worker]\ncpus=nan', 'node')
        self.assertNotIn('secret-canary', str(result))
        self.assertTrue(result['errors'])

    def test_dev_loopback_and_policy_warnings_are_distinct(self):
        self.assertEqual(lint('listen="[::1]:8443"\ndev=true', 'proxy', True)['errors'], [])
        self.assertTrue(lint('dev=true', 'proxy', True)['errors'])
        self.assertTrue(lint('dev=true', 'node')['errors'])
        result = lint('listen="0.0.0.0:8080"', 'node')
        self.assertEqual(result['errors'], [])
        self.assertIn('verify-private-node-ingress', [w['code'] for w in result['warnings']])
        self.assertEqual(lint('publish_listen="0.0.0.0:8444"', 'proxy')['errors'], [])
        self.assertTrue(lint('publish_listen="0.0.0.0:8444"', 'proxy')['warnings'])
        for text in ['listen="localhost:80"', 'listen="::1:80"', 'listen="127.0.0.1:65536"']:
            self.assertTrue(lint(text, 'node')['errors'])


if __name__ == '__main__':
    unittest.main()
