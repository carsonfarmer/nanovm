#!/usr/bin/env python3
"""Record requests to an app and replay them against another version, diffing the answers.

    tools/replay.py record --to http://127.0.0.1:8443 [--listen 127.0.0.1:9080] [--log session.jsonl]
    tools/replay.py replay session.jsonl --to http://127.0.0.1:8443 [--host new.localhost]
                    [--object-prefix r1-] [--ignore KEY]...

`record` is a small HTTP forwarder: point a client (or a test) at --listen
and it passes each request to --to unchanged, Host header included, and
appends the request and the answer to the log as one JSON line. Run it in
front of the proxy (tools/local-stack) or a node.

`replay` sends the logged requests in order to --to (optionally under another
Host: the new version's name) and compares each answer's status, the
`x-nanovm-error` header and the body with the recorded one. Objects keep
state, so the old answers only mean anything if the replay starts from the
same state: record a session against fresh objects, and replay it onto fresh
ones with --object-prefix (it renames `/{object}/...` to `/{prefix}{object}/...`).
`--ignore KEY` drops a top-level key from JSON bodies before comparing (a
timestamp, an instance counter, the app's name). Exits 1 if any answer differs.

Plain HTTP request/response only: WebSockets and streaming bodies are not
recorded.
"""

import argparse
import base64
import http.client
import http.server
import json
import sys
import threading
import urllib.parse

HOP = {"connection", "keep-alive", "te", "trailer", "transfer-encoding", "upgrade", "proxy-authorization",
       "proxy-connection", "content-length"}


def forward(to, method, path, headers, body):
    """Sends one request to `to` (http://host:port) and returns (status, headers, body)."""
    u = urllib.parse.urlsplit(to)
    conn = http.client.HTTPConnection(u.hostname, u.port or 80, timeout=60)
    try:
        conn.request(method, path, body=body, headers=headers)
        r = conn.getresponse()
        return r.status, [(k, v) for k, v in r.getheaders()], r.read()
    finally:
        conn.close()


def record(to, listen, log):
    lock = threading.Lock()
    out = open(log, "a")

    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def handle_one(self):
            n = int(self.headers.get("content-length") or 0)
            body = self.rfile.read(n) if n else b""
            headers = {k: v for k, v in self.headers.items() if k.lower() not in HOP}
            try:
                status, rh, rb = forward(to, self.command, self.path, headers, body)
            except OSError as e:
                status, rh, rb = 502, [("content-type", "text/plain")], f"replay.py: {e}".encode()
            entry = {"method": self.command, "path": self.path, "headers": headers,
                     "body": base64.b64encode(body).decode(), "status": status,
                     "error": next((v for k, v in rh if k.lower() == "x-nanovm-error"), None),
                     "response": base64.b64encode(rb).decode()}
            with lock:
                out.write(json.dumps(entry) + "\n")
                out.flush()
            self.send_response(status)
            for k, v in rh:
                if k.lower() not in HOP:
                    self.send_header(k, v)
            self.send_header("content-length", str(len(rb)))
            self.end_headers()
            self.wfile.write(rb)

        do_GET = do_POST = do_PUT = do_DELETE = do_PATCH = do_HEAD = do_OPTIONS = handle_one

        def log_message(self, *args):
            pass

    class Recorder(http.server.ThreadingHTTPServer):
        def server_close(self):
            super().server_close()
            out.close()

    server = Recorder(parse_addr(listen), Handler)
    print(f"recording to {log}: send requests to http://{listen}/ (forwarded to {to}); Ctrl-C stops", flush=True)
    return server


def parse_addr(s):
    host, port = s.rsplit(":", 1)
    return host, int(port)


def comparable(body, ignore):
    if not ignore:
        return body
    try:
        v = json.loads(body)
    except ValueError:
        return body
    if isinstance(v, dict):
        for k in ignore:
            v.pop(k, None)
    return json.dumps(v, sort_keys=True).encode()


def rename(path, prefix):
    if not prefix:
        return path
    rest = path[1:] if path.startswith("/") else path
    return "/" + prefix + rest


def replay(log, to, host=None, prefix="", ignore=(), out=sys.stdout):
    differ = total = 0
    with open(log) as f:
        for line in f:
            e = json.loads(line)
            total += 1
            headers = dict(e["headers"])
            if host:
                headers = {k: v for k, v in headers.items() if k.lower() != "host"}
                headers["Host"] = host
            path = rename(e["path"], prefix)
            status, rh, rb = forward(to, e["method"], path, headers, base64.b64decode(e["body"]))
            error = next((v for k, v in rh if k.lower() == "x-nanovm-error"), None)
            old = base64.b64decode(e["response"])
            if (status, error, comparable(rb, ignore)) != (e["status"], e["error"], comparable(old, ignore)):
                differ += 1
                print(f"DIFF {e['method']} {path}", file=out)
                print(f"  was {e['status']} {e['error'] or ''} {old[:300]!r}", file=out)
                print(f"  now {status} {error or ''} {rb[:300]!r}", file=out)
    print(f"{total} requests replayed, {differ} answered differently", file=out)
    return differ


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="command", required=True)
    r = sub.add_parser("record")
    r.add_argument("--to", required=True)
    r.add_argument("--listen", default="127.0.0.1:9080")
    r.add_argument("--log", default="session.jsonl")
    y = sub.add_parser("replay")
    y.add_argument("log")
    y.add_argument("--to", required=True)
    y.add_argument("--host")
    y.add_argument("--object-prefix", default="")
    y.add_argument("--ignore", action="append", default=[])
    a = p.parse_args()
    if a.command == "record":
        server = record(a.to, a.listen, a.log)
        try:
            server.serve_forever()
        except KeyboardInterrupt:
            pass
        finally:
            server.server_close()
    else:
        sys.exit(1 if replay(a.log, a.to, a.host, a.object_prefix, a.ignore) else 0)


if __name__ == "__main__":
    main()
