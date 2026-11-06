import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

from review_tools import changelog, license_review, oci_inventory, size_diff


class ReviewTests(unittest.TestCase):
    def test_size_digest_budget_and_toolchain(self):
        before, after = b'\0asm\x01\0\0\0', b'\0asm\x01\0\0\0\0\x02\x01x'
        result = size_diff(before, after, 'A', 'B', 12)
        self.assertEqual((result['before_bytes'], result['after_bytes'], result['delta_bytes']), (8, 12, 4))
        self.assertEqual(result['delta_percent'], 50)
        self.assertFalse(result['same_toolchain_label'])
        self.assertFalse(result['over_budget'])
        self.assertTrue(size_diff(before, after, 'A', 'A', 11)['over_budget'])
        self.assertNotEqual(result['before_sha256'], result['after_sha256'])
        self.assertEqual(size_diff(after, before, 'A', 'A', 8)['delta_bytes'], -4)
        with self.assertRaises(ValueError):
            size_diff(before, b'not wasm', 'A', 'A', 20)

    def test_licenses_preserve_expressions_omit_paths_and_urls(self):
        metadata = {'packages': [
            dict(name='z', version='1', license=None, license_file='/secret-canary/file',
                 source='git+https://secret-canary@private/repo'),
            dict(name='a', version='2', license='MIT OR Apache-2.0', license_file=None,
                 source='registry+https://secret-canary'),
            dict(name='b', version='1', license=None, license_file=None, source=None)]}
        result = license_review(metadata)
        self.assertEqual([p['name'] for p in result['packages']], ['a', 'b', 'z'])
        self.assertEqual(result['missing_declarations'], 1)
        self.assertEqual(result['packages'][0]['declared_license'], 'MIT OR Apache-2.0')
        self.assertNotIn('secret-canary', json.dumps(result))

    def test_changelog_explicit_range_and_ancestry(self):
        with tempfile.TemporaryDirectory() as directory:
            def git(*args):
                return subprocess.check_output(['git', '-C', directory, *args], text=True).strip()
            git('init', '-q')
            git('config', 'user.name', 'Fixture')
            git('config', 'user.email', 'fixture@example.invalid')
            commits = []
            for subject in ['base', 'first\u2028candidate', 'second candidate']:
                git('-c', 'commit.gpgsign=false', 'commit', '--allow-empty', '-qm', subject)
                commits.append(git('rev-parse', 'HEAD'))
            result = changelog(Path(directory), commits[0], commits[2])
            self.assertEqual([c['subject'] for c in result['candidates']], ['first\u2028candidate', 'second candidate'])
            self.assertEqual(changelog(Path(directory), commits[1], commits[1])['candidates'], [])
            with self.assertRaises(subprocess.CalledProcessError):
                changelog(Path(directory), commits[2], commits[0])
            with self.assertRaises(ValueError):
                changelog(Path(directory), '--all', 'HEAD')

    def test_oci_integrity_is_not_signature_verification(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'sha256').mkdir()
            body = b'synthetic data'
            digest = hashlib.sha256(body).hexdigest()
            path = root / 'sha256' / digest
            path.write_bytes(body)
            descriptor = dict(digest='sha256:' + digest, size=len(body), mediaType='application/octet-stream')
            manifest = dict(schemaVersion=2, mediaType='application/vnd.oci.image.manifest.v1+json',
                            config=descriptor, layers=[descriptor], annotations={'secret-canary': 'private'})
            raw = json.dumps(manifest).encode()
            result = oci_inventory(raw, root)
            self.assertEqual(len(result['descriptors']), 2)
            self.assertEqual(result['signature_status'], 'NOT VERIFIED')
            self.assertNotIn('secret-canary', json.dumps(result))
            path.write_bytes(b'X' * len(body))
            with self.assertRaises(ValueError):
                oci_inventory(raw, root)
            path.write_bytes(body + b'!')
            with self.assertRaises(ValueError):
                oci_inventory(raw, root)
            path.unlink()
            with self.assertRaises(FileNotFoundError):
                oci_inventory(raw, root)
            with tempfile.NamedTemporaryFile() as outside:
                path.symlink_to(outside.name)
                with self.assertRaises(ValueError):
                    oci_inventory(raw, root)


if __name__ == '__main__':
    unittest.main()
