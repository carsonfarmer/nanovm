#!/bin/sh
# Builds everything, starts a nanovm node with demo/nanovm.toml (unless one is
# already listening), and runs the dashboard's scenarios from the terminal
# while a canary object is pinged throughout. Needs curl and jq.
set -e
cd "$(dirname "$0")/.."
URL=${URL:-http://127.0.0.1:8080}
LOG=target/demo-serve.log

say() { printf '\n\033[1m%s\033[0m\n' "$*"; }
api() { curl -sf -XPOST "$URL/_nanovm/demo/$1"; }
# One line from a /demo/spawn or /demo/hammer result.
summary() {
    jq -r '"  \(.n) requests in \(.elapsed_ms | floor) ms: \(.rps | floor)/s, p50 \(.p50_us) µs, p99 \(.p99_us) µs, \(.ok) ok"'
}

say "building guests and the node"
scripts/build-guests.sh >/dev/null
cargo build --release -p nanovm 2>&1 | tail -1

SERVER=
if curl -sf "$URL/_nanovm/stats" >/dev/null 2>&1; then
    say "using the node already listening on $URL"
else
    say "starting a node on $URL (log: $LOG)"
    ./target/release/nanovm serve --config demo/nanovm.toml >"$LOG" 2>&1 &
    SERVER=$!
    until curl -sf "$URL/_nanovm/stats" >/dev/null 2>&1; do sleep 0.1; done
fi
echo "  dashboard: $URL/_nanovm/"

# The canary: one benign object, pinged every 20 ms for the whole demo.
CANARY=target/demo-canary.txt
: >"$CANARY"
(
    while :; do
        curl -s -o /dev/null -w '%{http_code} %{time_total}\n' "$URL/counter/canary/" >>"$CANARY"
        sleep 0.02
    done
) &
PINGER=$!
trap 'kill $PINGER 2>/dev/null; [ -n "$SERVER" ] && kill $SERVER 2>/dev/null; exit' INT TERM EXIT

say "every object is its own sandbox with its own /data"
curl -s -XPOST "$URL/counter/alice/incr" >/dev/null
echo "  alice: $(curl -s -XPOST "$URL/counter/alice/incr")"
echo "  bob:   $(curl -s "$URL/counter/bob/")"
echo "  path escapes: $(curl -s "$URL/hostile/demo/escape" | jq -c '[.[] | .result // .symlink] | unique')"
echo "  egress to example.com (allowlisted): $(curl -s "$URL/hostile/demo/fetch?url=http://example.com/" | jq -c '{status, bytes}')"
echo "  egress to 169.254.169.254: $(curl -s "$URL/hostile/demo/fetch?url=http://169.254.169.254/latest/meta-data/" | jq -c .)"

say "cold-starting 10,000 new objects"
api "spawn?n=10000" | tee target/demo-spawn.json | summary
jq -r '"  \(.live_after) live objects, RSS \(.rss_peak_mb | floor) MB, \(.rss_per_live_kb | floor) KB per object"' target/demo-spawn.json

say "hammering 1,000 warm objects (200,000 requests)"
api "hammer?n=200000&objects=1000&prefix=warm" | summary

say "writing /data on 1,000 objects (20,000 POST /incr)"
api "hammer?n=20000&objects=1000&prefix=warm&method=POST&path=/incr" | summary

say "one object serves one request at a time (20,000 requests, 1 client)"
api "hammer?n=20000&objects=1&concurrency=1&prefix=solo" | summary

say "attack storm: 10 rounds of every attack in the hostile guest, all at once"
api "storm?rounds=10" | jq -r '
    "  \(.attacks | map(.status | add) | add) attacks in \(.elapsed_ms | floor) ms",
    (.attacks[] | "  \(.attack | .+ " " * (16 - length)) \(.status | to_entries | map("\(.key)×\(.value)") | join(" ")) \(if .killed_by then "killed (\(.killed_by))" else "refused" end), slowest \(.max_ms | floor) ms")'

kill $PINGER 2>/dev/null
wait $PINGER 2>/dev/null || true
say "the canary, pinged throughout"
sort -n -k2 "$CANARY" | awk '{n++; if ($1 != 200) bad++; t[n] = $2 * 1000}
    END {printf "  %d requests over HTTP, %d not 200, p50 %.1f ms, p99 %.1f ms (curl per request)\n",
        n, bad, t[int(n * 0.5) + 1], t[int(n * 0.99) + 1]}'
curl -s "$URL/_nanovm/stats" | jq -r '"  node: \(.live_objects) live objects, \(.instantiations) instantiations, \(.poisons | to_entries | map(select(.value > 0) | "\(.key) \(.value)") | join(", ")) killed"'

if [ -n "$SERVER" ]; then
    say "node still running: $URL/_nanovm/ (Ctrl-C to stop)"
    trap 'kill $SERVER 2>/dev/null; exit' INT TERM EXIT
    wait $SERVER
fi
