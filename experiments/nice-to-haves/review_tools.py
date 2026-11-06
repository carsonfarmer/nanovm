"""Offline review aids: candidates and integrity evidence, never release approval."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys

from toolkit import inspect_wasm


def size_diff(before, after, before_toolchain, after_toolchain, budget):
    if not all(isinstance(x, str) and x.strip() for x in (before_toolchain, after_toolchain)):
        raise ValueError('explicit toolchain descriptions required')
    if type(budget) is not int or budget < 0:
        raise ValueError('nonnegative byte budget required')
    old, new = inspect_wasm(before), inspect_wasm(after)
    return {'before_sha256': hashlib.sha256(before).hexdigest(),
            'after_sha256': hashlib.sha256(after).hexdigest(),
            'before_toolchain': before_toolchain, 'after_toolchain': after_toolchain,
            'same_toolchain_label': before_toolchain == after_toolchain,
            'before_bytes': len(before), 'after_bytes': len(after),
            'delta_bytes': len(after) - len(before),
            'delta_percent': 100 * (len(after) - len(before)) / len(before),
            'over_budget': len(after) > budget,
            'before_sections': old['sections'], 'after_sections': new['sections'],
            'claim': 'encoded size only; not speed, validity or host compatibility'}


def license_review(metadata):
    rows = []
    for package in metadata['packages']:
        license = package.get('license')
        license_file = bool(package.get('license_file'))
        source = package.get('source') or ''
        rows.append({'name': package['name'], 'version': package['version'],
                     'source_kind': 'registry' if source.startswith('registry+') else
                                    'git' if source.startswith('git+') else 'local-or-other',
                     'declared_license': license, 'has_license_file': license_file,
                     'review': 'inspect license file' if license_file else
                               'missing declaration' if not license else 'review expression'})
    return {'packages': sorted(rows, key=lambda x: (x['name'], x['version'], x['source_kind'])),
            'scope': 'Cargo metadata declarations, not resolved build SBOM or legal advice',
            'missing_declarations': sum(not x['declared_license'] and not x['has_license_file'] for x in rows)}


def changelog(repo, base, head):
    if any(not re.fullmatch(r'[0-9a-f]{40}', commit) for commit in (base, head)):
        raise ValueError('provide explicit full lowercase SHA-1 commit IDs')
    def git(*args):
        return subprocess.run(['git', '-C', str(repo), *args], check=True,
                              capture_output=True, text=True, timeout=30).stdout
    # No implicit HEAD, remote fetch, publication or release-tag creation.
    if any(git('cat-file', '-t', commit).strip() != 'commit' for commit in (base, head)):
        raise ValueError('range endpoints must be commits')
    git('merge-base', '--is-ancestor', base, head)
    fields = git('log', '-z', '--reverse', '--format=%H%x00%s', f'{base}..{head}').split('\0')[:-1]
    return {'base': base, 'head': head,
            'candidates': [{'commit': commit, 'subject': subject}
                           for commit, subject in zip(fields[::2], fields[1::2])],
            'review_required': True, 'warning': 'subjects may contain private text; review before sharing'}


def oci_inventory(manifest_bytes, blobs):
    if len(manifest_bytes) > 4 * 1024 * 1024:
        raise ValueError('manifest exceeds 4 MiB prototype limit')
    manifest = json.loads(manifest_bytes)
    if type(manifest.get('schemaVersion')) is not int or manifest['schemaVersion'] != 2:
        raise ValueError('OCI schemaVersion 2 required')
    if manifest.get('mediaType') != 'application/vnd.oci.image.manifest.v1+json':
        raise ValueError('only OCI image manifests supported, not indexes')
    if not isinstance(manifest['layers'], list) or len(manifest['layers']) > 128:
        raise ValueError('at most 128 layers required')
    root = blobs.resolve()
    entries = []
    for descriptor in [manifest['config'], *manifest['layers']]:
        digest, size = descriptor['digest'], descriptor['size']
        if not isinstance(digest, str) or not re.fullmatch(r'sha256:[0-9a-f]{64}', digest):
            raise ValueError('only lowercase sha256 descriptors supported')
        if type(size) is not int or not 0 <= size <= 64 * 1024 * 1024:
            raise ValueError('blob exceeds 64 MiB prototype limit or invalid size')
        path = (root / 'sha256' / digest[7:]).resolve()
        if not path.is_relative_to(root):
            raise ValueError('blob escapes inventory root')
        hasher, actual = hashlib.sha256(), 0
        with path.open('rb') as stream:
            while chunk := stream.read(65536):
                actual += len(chunk)
                if actual > size:
                    raise ValueError('blob exceeds descriptor size')
                hasher.update(chunk)
        if actual != size or hasher.hexdigest() != digest[7:]:
            raise ValueError('blob size or digest mismatch')
        entries.append({'digest': digest, 'bytes': actual, 'media_type': descriptor['mediaType']})
    return {'manifest_sha256': hashlib.sha256(manifest_bytes).hexdigest(),
            'descriptors': entries, 'signature_status': 'NOT VERIFIED',
            'claim': 'local descriptor integrity only; no signer, policy, registry or runtime verification'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    sizes = commands.add_parser('size')
    sizes.add_argument('before', type=Path)
    sizes.add_argument('after', type=Path)
    sizes.add_argument('--before-toolchain', required=True)
    sizes.add_argument('--after-toolchain', required=True)
    sizes.add_argument('--budget', type=int, required=True)
    licenses = commands.add_parser('licenses')
    licenses.add_argument('metadata', type=Path)
    changes = commands.add_parser('changelog')
    changes.add_argument('repo', type=Path)
    changes.add_argument('base')
    changes.add_argument('head')
    oci = commands.add_parser('oci')
    oci.add_argument('manifest', type=Path)
    oci.add_argument('blobs', type=Path)
    args = parser.parse_args()
    try:
        if args.command == 'size':
            result = size_diff(args.before.read_bytes(), args.after.read_bytes(),
                               args.before_toolchain, args.after_toolchain, args.budget)
        elif args.command == 'licenses':
            result = license_review(json.loads(args.metadata.read_text()))
        elif args.command == 'changelog':
            result = changelog(args.repo, args.base, args.head)
        else:
            result = oci_inventory(args.manifest.read_bytes(), args.blobs)
        print(json.dumps(result, sort_keys=True, indent=2))
        return 0
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError):
        print('invalid input or unavailable evidence; no approval issued', file=sys.stderr)
        return 2


if __name__ == '__main__':
    sys.exit(main())
