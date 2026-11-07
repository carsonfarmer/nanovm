"""Read-only TOML lint prototype pinned to a reviewed configuration source snapshot."""

import argparse
import hashlib
import ipaddress
import json
import math
from pathlib import Path
import sys
import tomllib

SOURCE_SHA256 = '4726c8b3f7039a04d1933fe0768ba5e35d1fb43a8b7449fb521cb7d0f7f74d38'
LIMITS = dict.fromkeys('memory_mb cpu_ms request_timeout_ms post_return_timeout_ms idle_timeout_ms '
                       'max_request_body_kb max_response_body_kb disk_mb max_files max_queue '
                       'max_outbound_concurrency max_resources'.split(), 'u64') | {'egress': 'strings'}
WORKER = {'memory_mb': 'u64', 'cpus': 'number', 'pids': 'u64'}
NODE = (dict.fromkeys('listen data_dir publish_to telemetry_endpoint cgroup'.split(), 'string') |
        dict.fromkeys('max_live_objects memory_reservation_mb epoch_tick_ms heavy_lanes front_threads '
                      'max_component_mb max_apps load_timeout_ms'.split(), 'u64') |
        {'jail_uid_base': 'u32', 'dev': 'bool', 'limits': LIMITS, 'worker': WORKER})
PROXY = (dict.fromkeys('listen node publish_listen telemetry_endpoint resolver'.split(), 'string') |
         dict.fromkeys('max_sockets max_sockets_per_app max_sockets_per_object max_request_body_kb '
                       'max_response_body_kb socket_backlog_kb tag_ttl_s'.split(), 'u64') |
         {'dev': 'bool'})


def address(value):
    host, port = value.rsplit(':', 1)
    if host.startswith('[') and host.endswith(']'):
        host = host[1:-1]
        if ':' not in host:
            raise ValueError('brackets require IPv6')
    elif ':' in host:
        raise ValueError('IPv6 needs brackets')
    if not port.isascii() or not port.isdecimal() or not 0 <= int(port) <= 65535:
        raise ValueError('invalid port')
    return ipaddress.ip_address(host)


def lint(text, kind, dev_build=False):
    if kind not in ('node', 'proxy'):
        raise ValueError('node or proxy required')
    data = tomllib.loads(text)
    errors, warnings = [], []

    def check(table, schema, prefix=''):
        for key, value in table.items():
            field = prefix + key
            rule = schema.get(key)
            if rule is None:
                errors.append({'field': field, 'code': 'unknown-field'})
                continue
            if isinstance(rule, dict):
                if isinstance(value, dict):
                    check(value, rule, field + '.')
                    continue
                valid = False
            elif rule.startswith('u'):
                valid = type(value) is int and 0 <= value < 2**int(rule[1:])
            elif rule == 'string':
                valid = isinstance(value, str)
            elif rule == 'bool':
                valid = type(value) is bool
            elif rule == 'strings':
                valid = isinstance(value, list) and all(isinstance(x, str) for x in value)
            else:
                valid = type(value) in (int, float) and math.isfinite(value) and value >= 0
            if not valid:
                errors.append({'field': field, 'code': 'invalid-type-or-range'})
    check(data, NODE if kind == 'node' else PROXY)
    try:
        listen = address(data.get('listen', '127.0.0.1:8080' if kind == 'node' else '0.0.0.0:8443'))
        if data.get('dev') is True:
            if not dev_build:
                errors.append({'field': 'dev', 'code': 'requires-dev-build'})
            if not listen.is_loopback:
                errors.append({'field': 'listen', 'code': 'dev-requires-loopback'})
        if kind == 'node' and not listen.is_loopback:
            warnings.append({'field': 'listen', 'code': 'verify-private-node-ingress'})
    except (ValueError, TypeError, AttributeError):
        errors.append({'field': 'listen', 'code': 'invalid-socket-address'})
    if kind == 'proxy':
        try:
            if not address(data.get('publish_listen', '127.0.0.1:8444')).is_loopback:
                warnings.append({'field': 'publish_listen', 'code': 'verify-private-control-ingress'})
        except (ValueError, TypeError, AttributeError):
            warnings.append({'field': 'publish_listen', 'code': 'unverified-control-address'})
    if kind == 'node' and 'cgroup' not in data:
        warnings.append({'field': 'cgroup', 'code': 'worker-cgroup-enforcement-not-configured'})
    return {'errors': errors, 'warnings': warnings, 'source_sha256': SOURCE_SHA256,
            'scope': 'snapshot syntax/types and selected policy checks, not deployment approval'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('kind', choices=['node', 'proxy'])
    parser.add_argument('config', type=Path)
    parser.add_argument('--dev-build', action='store_true', help='caller assertion, not binary inspection')
    parser.add_argument('--source', type=Path, required=True, help='reviewed crates/nanovm/src/config.rs')
    args = parser.parse_args()
    try:
        if hashlib.sha256(args.source.read_bytes()).hexdigest() != SOURCE_SHA256:
            raise ValueError('configuration source drifted: review schema before use')
        result = lint(args.config.read_text(), args.kind, args.dev_build)
        print(json.dumps(result, indent=2, sort_keys=True))
        return int(bool(result['errors']))
    except (ValueError, OSError):
        print('unreadable/invalid TOML or source drift; no configuration values printed', file=sys.stderr)
        return 2


if __name__ == '__main__':
    sys.exit(main())
