#!/bin/bash
# The single-node reference end to end, inside a throwaway systemd container
# (run by deploy/systemd/test/nspawn.sh with SCRIPT= this file, MODES=plain):
# install.sh, Caddy (internal CA) -> nanovm proxy -> node -> jailed worker, a
# DNS-over-HTTPS stand-in answering the app's TXT record, the loopback dev
# registry, and a counting OTLP sink on the Collector's port. Dev mode: apps are
# unsigned, so the binary must be a dev build.
set -u
HERE=$(cd "$(dirname "$0")/.." && pwd)
fail=0
check() { if eval "$2"; then echo "PASS $1"; else echo "FAIL $1"; fail=1; fi; }
W=/run/e2e; mkdir -p $W

# The DoH stand-in, the dev registry and the Collector stand-in.
cat >$W/doh.py <<'PY'
import json
from http.server import HTTPServer, BaseHTTPRequestHandler
TXT = {"_nanovm.app.test": "app=127.0.0.1:5000/nanovm/counter:latest; trust=dev dev"}
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        name = self.path.split("name=", 1)[1].split("&")[0] if "name=" in self.path else ""
        ans = [{"name": name + ".", "type": 16, "TTL": 60, "data": '"%s"' % TXT[name]}] if name in TXT else []
        body = json.dumps({"Status": 0, "Answer": ans}).encode()
        self.send_response(200); self.send_header("content-type", "application/dns-json")
        self.send_header("content-length", str(len(body))); self.end_headers(); self.wfile.write(body)
HTTPServer(("127.0.0.1", 5353), H).serve_forever()
PY
cat >$W/sink.py <<'PY'
import json
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
n = {}
class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length", 0)))
        for name in (b"proxy.request", b"supervisor.request", b"request"):
            k = name.decode(); n[k] = n.get(k, 0) + body.count(b"\x2a" + bytes([len(name)]) + name)
        self.send_response(200); self.send_header("content-length", "0"); self.end_headers()
    def do_GET(self):
        b = json.dumps(n).encode()
        self.send_response(200); self.send_header("content-length", str(len(b))); self.end_headers(); self.wfile.write(b)
ThreadingHTTPServer(("127.0.0.1", 4318), H).serve_forever()
PY
systemd-run -q --unit=doh python3 -I $W/doh.py
systemd-run -q --unit=collector python3 -I $W/sink.py
systemd-run -q --unit=devreg /usr/local/bin/dev-registry

# The reference install, in dev mode, with the resolver pointed at the stand-in.
# This test's container has no cgroup v2 hierarchy: its node config, the
# reference with workers without [worker] limits, is in place first, and
# install.sh (and its host check) keeps it.
install -d /etc/nanovm && sed '1i cgroup = "off"' "$HERE/../systemd/nanovm.toml" >/etc/nanovm/nanovm.toml
check "install.sh (and its host check)" '"$HERE/install.sh" /usr/local/bin/nanovm >$W/install.log 2>&1'
sed -n '/^  /p;/^WARN\|^FAIL/p' $W/install.log
sed -i '1i dev = true' /etc/nanovm/nanovm.toml
sed -i -e '1i dev = true' -e 's|^resolver = .*|resolver = "http://127.0.0.1:5353/dns-query"|' \
    -e 's|^hosts = .*|hosts = ["app.test", "other.test"]|' -e 's|^signers = .*|signers = ["dev dev"]|' /etc/nanovm/proxy.toml
sed -e 's/^app.example.com, www.app.example.com {/app.test {\n\ttls internal/' -e 's/^\temail ops@example.com/\tskip_install_trust/' \
    "$HERE/Caddyfile" >/etc/caddy/Caddyfile
systemctl enable -q --now nanovm nanovm-proxy
# The test root has an empty /var/lib; the caddy package made this on the host.
install -d -o caddy -g caddy /var/lib/caddy
systemctl restart caddy || journalctl -u caddy --no-pager -n 15 | grep -o 'Error.*'
CA=/var/lib/caddy/.local/share/caddy/pki/authorities/local/root.crt
for _ in $(seq 100); do [ -s $CA ] && curl -s -o /dev/null --cacert $CA --resolve app.test:443:127.0.0.1 https://app.test/ && break; sleep 0.2; done
c() { curl -s --cacert $CA --resolve app.test:443:127.0.0.1 "$@"; }
check "caddy, proxy and node active" '[ "$(systemctl is-active caddy nanovm nanovm-proxy | sort -u)" = active ]'
for _ in 1 2 3; do c -XPOST https://app.test/alice/incr; echo; done
check "https://app.test/alice/ through Caddy (verified TLS), proxy, node: count 3" 'c https://app.test/alice/ | grep -q "\"count\":3"'
check "a client cannot name the app itself" '[ "$(c -o /dev/null -w "%{http_code}" -H "nanovm-app: x" https://app.test/alice/)" = 200 ]'
check "a host with no TXT record: 404 from the proxy" '[ "$(curl -s -o /dev/null -w "%{http_code}" -H "Host: other.test" http://127.0.0.1:8443/alice/)" = 404 ]'
check "a host the proxy doesn't serve: 403" '[ "$(curl -s -o /dev/null -w "%{http_code}" -H "Host: evil.test" http://127.0.0.1:8443/alice/)" = 403 ]'
# The node (8080) and publish (8444) listeners: present, and on 127.0.0.1 only.
loopback_only() { python3 -c 'import sys
seen = set()
import os
for f in ("/proc/net/tcp", "/proc/net/tcp6"):
    for l in (open(f).read().splitlines()[1:] if os.path.exists(f) else []):
        addr, port = l.split()[1].split(":")
        if l.split()[3] == "0A" and int(port, 16) in (8080, 8444):
            seen.add((addr, int(port, 16)))
sys.exit(0 if seen == {("0100007F", 8080), ("0100007F", 8444)} else 1)'; }
check "node and publish ports listen on 127.0.0.1 only" loopback_only
sleep 7   # export interval and batch
spans=$(curl -s http://127.0.0.1:4318/)
echo "Collector stand-in received: $spans"
check "spans from all three layers reach the Collector port" 'python3 -c "import json,sys; d=json.loads(sys.argv[1]); sys.exit(0 if all(d.get(k,0)>=3 for k in (\"proxy.request\",\"supervisor.request\",\"request\")) else 1)" "$spans"'
systemctl restart nanovm
for _ in $(seq 50); do curl -sf http://127.0.0.1:8080/_nanovm/ready >/dev/null && break; sleep 0.2; done
check "count survives a node restart" 'c https://app.test/alice/ | grep -q "\"count\":3"'
systemctl stop caddy nanovm-proxy nanovm doh collector devreg
echo "RESULT fail=$fail"
