"""Real loopback wire-byte and lost-reply exercise; no external target accepted."""

from http.server import BaseHTTPRequestHandler, HTTPServer
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading

from http_operation import attempt
from protocol_models import ChatRoom


def exercise():
    node = shutil.which('node')
    if node is None:
        raise RuntimeError('Node.js required for wire exercise')
    room = ChatRoom()
    seen = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            raw = self.rfile.read(int(self.headers['Content-Length']))
            operation = self.headers['Idempotency-Key']
            seen.append((operation, raw))
            result = room.send(0, 'authenticated-fixture-caller', operation, raw.decode('utf-8'))
            if self.path == '/lost':
                self.close_connection = True  # Application accepted, response never sent.
                return
            self.send_response(409 if result['status'] == 'conflict' else 200)
            self.send_header('Content-Length', '0')
            self.end_headers()

    body = '{ "message": "é e\u0301 🌍", "count": 1 }\n'.encode('utf-8')
    with HTTPServer(('127.0.0.1', 0), Handler) as server, tempfile.TemporaryDirectory() as directory:
        thread = threading.Thread(target=server.serve_forever)
        thread.start()
        try:
            url = f'http://127.0.0.1:{server.server_port}'
            path = Path(directory) / 'body.json'
            path.write_bytes(body)
            def js(route):
                run = subprocess.run([node, str(Path(__file__).with_name('operation_client.mjs')),
                                      url + route, 'operation-1', str(path)], check=True,
                                     capture_output=True, text=True, timeout=10)
                return json.loads(run.stdout)
            results = [attempt(url + '/lost', 'operation-1', body), js('/lost'),
                       attempt(url + '/ok', 'operation-1', body), js('/ok')]
            path.write_bytes(body + b' ')  # Same JSON meaning is not the same wire body.
            results.append(js('/ok'))
            expected = [('operation-1', body)] * 4 + [('operation-1', body + b' ')]
            checks = {
                'exact_wire_bytes': seen == expected,
                'both_lost_responses_unknown': [r['outcome'] for r in results[:2]] == ['unknown'] * 2,
                'explicit_retries_succeeded': [r.get('status') for r in results[2:4]] == [200, 200],
                'changed_bytes_conflict': results[4].get('status') == 409,
                'one_application_event': len(room.replay(0)['events']) == 1,
            }
            if not all(checks.values()):
                raise AssertionError(checks)
            return {'basis': 'synthetic local HTTP', 'requests': len(seen), 'checks': checks,
                    'node_version': subprocess.check_output([node, '--version'], text=True).strip()}
        finally:
            server.shutdown()
            thread.join(timeout=5)


if __name__ == '__main__':
    print(json.dumps(exercise(), indent=2, sort_keys=True))
