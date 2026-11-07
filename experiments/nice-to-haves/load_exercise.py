"""Bounded rate/concurrency/skew exercise against its OWN loopback server only."""

import argparse
from concurrent.futures import ThreadPoolExecutor
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import threading
import time
import urllib.error
import urllib.request

from planning import calculate, integer, number


def exercise(plan, concurrency, rate, service_ms=30, fail_every=0):
    integer(concurrency, 1, 8)
    if not 1 <= number(rate) <= 200:
        raise ValueError('rate must be 1–200 requests/s')
    if not 0 <= number(service_ms) <= 200:
        raise ValueError('synthetic service delay must be 0–200 ms')
    integer(fail_every, 0, 200)
    integer(plan['requests'], 1, 200)
    if plan.get('kind') != 'popularity':
        raise ValueError('popularity plan required')
    sequence = calculate(plan)['result']['sequence']
    lock, rate_lock = threading.Lock(), threading.Lock()
    seen, starts = [], []
    active = peak = 0

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_GET(self):
            nonlocal active, peak
            object_id = int(self.path[1:])
            with lock:
                seen.append(object_id)
                index = len(seen)
                active += 1
                peak = max(peak, active)
            try:
                time.sleep(service_ms / 1000)
            finally:
                with lock:
                    active -= 1
            # Service ends before replying; do not count post-response teardown as load.
            body = str(object_id).encode()
            self.send_response(503 if fail_every and index % fail_every == 0 else 200)
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    server.daemon_threads = False
    thread = threading.Thread(target=server.serve_forever)
    thread.start()
    try:
        def get(object_id):
            # Never catch up missed slots in a burst. Saturation reduces offered rate.
            with rate_lock:
                if starts:
                    time.sleep(max(0, starts[-1] + 1 / rate - time.monotonic()))
                starts.append(time.monotonic())
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            try:
                response = opener.open(f'http://127.0.0.1:{server.server_port}/{object_id}', timeout=2)
            except urllib.error.HTTPError as error:
                response = error
            with response:
                return response.status, response.read() == str(object_id).encode()
        with ThreadPoolExecutor(max_workers=concurrency) as pool:
            results = list(pool.map(get, sequence))
    finally:
        server.shutdown()
        server.server_close()
        thread.join()
    expected_errors = len(sequence) // fail_every if fail_every else 0
    errors = sum(status != 200 or not matches for status, matches in results)
    if sorted(seen) != sorted(sequence) or errors != expected_errors or peak > concurrency:
        raise AssertionError('fixture observations violate load contract')
    return {'basis': 'synthetic handler with real local requests, not nanovm performance',
            'requests': len(results), 'errors': errors, 'server_peak_active': peak,
            'concurrency_cap': concurrency, 'rate_cap': rate,
            'minimum_start_spacing_s': min((b - a for a, b in zip(starts, starts[1:])), default=None),
            'observed_object_counts': [seen.count(i) for i in range(plan['objects'])]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('plan', type=argparse.FileType('r'))
    parser.add_argument('--concurrency', type=int, default=2)
    parser.add_argument('--rate', type=float, default=20)
    parser.add_argument('--service-ms', type=float, default=30)
    parser.add_argument('--fail-every', type=int, default=0)
    args = parser.parse_args()
    with args.plan:
        result = exercise(json.load(args.plan), args.concurrency, args.rate, args.service_ms, args.fail_every)
    print(json.dumps(result, indent=2, sort_keys=True))


if __name__ == '__main__':
    main()
