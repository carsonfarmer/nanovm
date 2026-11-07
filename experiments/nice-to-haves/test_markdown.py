import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


@unittest.skipUnless(os.environ.get('NICE_MARKDOWN_TOOLS'), 'pinned isolated Markdown packages required')
class MarkdownTests(unittest.TestCase):
    def check(self, root):
        result = subprocess.run(['node', str(Path(__file__).with_name('markdown_check.mjs')),
                                 str(root), str(root / 'index.md')], capture_output=True, text=True,
                                timeout=10)
        self.assertIn(result.returncode, [0, 1], result.stderr)
        return json.loads(result.stdout)

    def test_references_nested_links_code_and_duplicate_unicode_headings(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'with space.md').write_text('# Hello *World*!\n# Hello World!\n# Café `x`\n')
            (root / 'nested(x).md').write_text('# Nested\n')
            (root / 'index.md').write_text('[first](with%20space.md#hello-world)\n'
                '[second][REF]\n\n[REF]: with%20space.md#hello-world-1\n[REF]: missing\n\n'
                '[nested](nested(x).md#nested)\n'
                '[unicode](with%20space.md#caf%C3%A9-x)\n'
                '`[code](missing)`\n```md\n[fence](missing)\n```\n')
            result = self.check(root)
            self.assertEqual(result['checked'], 4)
            self.assertEqual(result['problems'], [])

    def test_missing_anchor_path_symlink_and_html_limit(self):
        with tempfile.TemporaryDirectory() as directory, tempfile.NamedTemporaryFile() as outside:
            root = Path(directory)
            (root / 'escape').symlink_to(outside.name)
            (root / 'index.md').write_text('# Present\n[ok](#present)\n[bad](#absent)\n'
                                          '[missing](missing.md)\n[escape](escape)\n'
                                          '<a id="custom"></a>\n[html](#custom)\n')
            result = self.check(root)
            self.assertEqual(len(result['problems']), 4)
            self.assertGreater(result['unsupported_html_or_nonmarkdown_anchors'], 0)


if __name__ == '__main__':
    unittest.main()
