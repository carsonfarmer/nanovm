#!/bin/bash
# Collector outage drill (runbooks.md): warm GETs with the Collector up (a
# counting sink), refused (closed port) and blackholed (unroutable address).
# Needs oha (OHA=path) and a dev build:
#   docs/ops/drills/collector-outage.sh target/debug
set -u
BIN=$1; PORT=28480; REG=25480; SINK=24318
OHA=${OHA:-oha}
W=$(mktemp -d /var/tmp/nanovm-otel.XXXXXX); chmod 755 $W
pids=()
trap 'kill "${pids[@]}" 2>/dev/null; wait 2>/dev/null; rm -rf $W' EXIT
cat > $W/sink.py <<'PY'
import json, sys
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
n = {"request": 0}
class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length", 0)))
        if self.path == "/v1/traces":
            n["request"] += body.count(b"\x2a\x07request")  # Span.name == "request"
        self.send_response(200); self.send_header("content-length", "0"); self.end_headers()
    def do_GET(self):
        b = json.dumps(n).encode()
        self.send_response(200); self.send_header("content-length", str(len(b))); self.end_headers(); self.wfile.write(b)
ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PY
python3 -I $W/sink.py $SINK & pids+=($!)
$BIN/examples/dev-registry 127.0.0.1:$REG > $W/apps 2>/dev/null & pids+=($!)
sleep 2
APP=$(awk '$1=="counter"{print $2}' $W/apps)
for round in 1 2; do
  for v in up refused blackhole; do
    case $v in up) ep=http://127.0.0.1:$SINK;; refused) ep=http://127.0.0.1:9;; blackhole) ep=http://10.255.255.1:4318;; esac
    rm -rf $W/data
    printf 'listen = "127.0.0.1:%s"\ndata_dir = "%s/data"\ndev = true\ncgroup = "off"\ntelemetry_endpoint = "%s"\n' $PORT $W $ep > $W/n.toml
    env -u RUST_BACKTRACE $BIN/nanovm serve --config $W/n.toml > $W/n-$v.log 2>&1 & np=$!
    sleep 1
    "$OHA" --no-tui -n 2000 -c 16 -H "nanovm-app: $APP" -H 'nanovm-trust: dev dev' --rand-regex-url "http://127.0.0.1:$PORT/o[0-9]{2}/" >/dev/null
    c0=$(curl -s http://127.0.0.1:$SINK/count | python3 -c 'import json,sys; print(json.load(sys.stdin)["request"])')
    res=$("$OHA" --no-tui --output-format json -n 20000 -c 16 -H "nanovm-app: $APP" -H 'nanovm-trust: dev dev' \
      --rand-regex-url "http://127.0.0.1:$PORT/o[0-9]{2}/" | python3 -c 'import json,sys; o=json.load(sys.stdin); l=o["latencyPercentiles"]
print(round(o["summary"]["requestsPerSec"]), round(l["p50"]*1e3,2), round(l["p99"]*1e3,2), o["statusCodeDistribution"])')
    sleep 3
    c1=$(curl -s http://127.0.0.1:$SINK/count | python3 -c 'import json,sys; print(json.load(sys.stdin)["request"])')
    kill $np; wait $np 2>/dev/null
    warn=$(grep -c 'OTLP\|Export' $W/n-$v.log)
    echo "round $round $v: rps p50 p99 codes = $res; request spans at the sink: $((c1 - c0)); export warnings on stderr: $warn; load $(cut -d' ' -f1 /proc/loadavg)"
  done
done
grep -h 'OTLP\|Export' $W/n-blackhole.log | sed 's/\x1b\[[0-9;]*m//g' | cut -c1-200 | sort | uniq -c | sort -rn | head -3
