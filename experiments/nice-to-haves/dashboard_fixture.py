"""Synthetic Prometheus metrics for the example dashboard, never host telemetry."""

from http.server import BaseHTTPRequestHandler, HTTPServer
import os
import time


def metrics(seconds):
    lines = ['# TYPE nice_requests_total counter']
    for layer in ('supervisor', 'proxy'):
        for outcome, rate in [('ok', 80), ('error', 20)]:
            lines.append(f'nice_requests_total{{layer="{layer}",outcome="{outcome}"}} {seconds * rate}')
    lines += ['# TYPE nice_collector_queue_items gauge', 'nice_collector_queue_items 7',
              '# TYPE nice_collector_queue_capacity gauge', 'nice_collector_queue_capacity 20',
              '# TYPE nice_collector_dropped_spans_total counter',
              f'nice_collector_dropped_spans_total {seconds * 5}']
    return '\n'.join(lines) + '\n'


if __name__ == '__main__':
    start = time.monotonic()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_GET(self):
            if self.path != '/metrics':
                self.send_error(404)
                return
            body = metrics(time.monotonic() - start).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'text/plain; version=0.0.4')
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    with HTTPServer(('127.0.0.1', int(os.environ.get('PORT', '8790'))), Handler) as server:
        server.serve_forever()
