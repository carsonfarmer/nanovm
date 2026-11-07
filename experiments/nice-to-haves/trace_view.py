"""Normalize OTLP JSON into local span aliases (not anonymization) and an HTML view."""

import argparse
import json
from pathlib import Path
import re


def normalize(document):
    traces = {}
    count = 0
    for resource in document.get('resourceSpans', []):
        for scope in resource.get('scopeSpans', []):
            for span in scope.get('spans', []):
                count += 1
                if count > 1000:
                    raise ValueError('at most 1000 spans supported')
                trace, identity, parent = span['traceId'], span['spanId'], span.get('parentSpanId', '')
                identifiers = [(trace, 32), (identity, 16)]
                if parent != '':
                    identifiers.append((parent, 16))
                for value, size in identifiers:
                    if not isinstance(value, str) or not re.fullmatch('[0-9a-fA-F]{' + str(size) + '}', value) or int(value, 16) == 0:
                        raise ValueError('nonzero hex trace/span IDs required')
                trace, identity, parent = trace.lower(), identity.lower(), parent.lower()
                times = []
                for key in ('startTimeUnixNano', 'endTimeUnixNano'):
                    raw = span[key]
                    if not isinstance(raw, str) or not re.fullmatch(r'[0-9]{1,20}', raw) or int(raw) >= 2**64:
                        raise ValueError('timestamps must be uint64 decimal strings')
                    times.append(int(raw))
                start, end = times
                if end < start:
                    raise ValueError('negative span duration')
                group = traces.setdefault(trace, {})
                if identity in group:
                    raise ValueError('duplicate span identity')
                group[identity] = dict(parent=parent, start=start, end=end)
    output = []
    for trace, spans in sorted(traces.items()):
        order = sorted(spans, key=lambda key: (spans[key]['start'], key))
        aliases = {key: f'Span {i + 1}' for i, key in enumerate(order)}
        origin = min(s['start'] for s in spans.values())
        rows = []
        for identity in order:
            span = spans[identity]
            ancestors, current = {identity}, span['parent']
            while current in spans:
                if current in ancestors:
                    raise ValueError('parent cycle')
                ancestors.add(current)
                current = spans[current]['parent']
            parent = spans.get(span['parent'])
            rows.append(dict(label=aliases[identity], parent=aliases.get(span['parent']),
                             missing_parent=bool(span['parent']) and parent is None,
                             outside_parent=bool(parent and (span['start'] < parent['start'] or span['end'] > parent['end'])),
                             start_ms=(span['start'] - origin) / 1e6,
                             duration_ms=(span['end'] - span['start']) / 1e6))
        output.append(dict(label=f'Trace {len(output) + 1}', spans=rows))
    return output


def render(document):
    data = normalize(document)
    template = Path(__file__).with_name('trace_template.html').read_text()
    return template.replace('__TRACE_DATA__', json.dumps(data, allow_nan=False))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('input', type=Path)
    parser.add_argument('output', type=Path, help='new HTML file; refuses overwrite')
    args = parser.parse_args()
    html = render(json.loads(args.input.read_text()))
    with args.output.open('x') as output:
        output.write(html)
