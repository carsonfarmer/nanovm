#!/bin/sh
# Builds everything, starts a nanovm node with demo/nanovm.toml (unless one is
# already listening), and drives it over HTTP with curl while a canary object
# is pinged throughout. Needs curl (7.84+) and jq. These numbers include
# curl and the HTTP front end; `cargo run --release --example bench` measures
# the node in-process.
set -e
cd "$(dirname "$0")/.."
URL=${URL:-http://127.0.0.1:8080}
LOG=target/demo-serve.log
mkdir -p target

say() { printf '\n\033[1m%s\033[0m\n' "$*"; }

# blast N CONCURRENCY MOD PATH-FORMAT [curl args...]: N requests, request i to
# $URL/<PATH-FORMAT with %d = i % MOD>, then one summary line.
blast() {
    n=$1 conc=$2 mod=$3 fmt=$4
    shift 4
    awk -v n="$n" -v m="$mod" -v f="$URL/$fmt" \
        'BEGIN { for (i = 0; i < n; i++) printf "url = \"" f "\"\noutput = \"/dev/null\"\n", i % m }' >target/demo-blast.cfg
    t0=$(date +%s%N)
    curl -s -Z --parallel-max "$conc" -K target/demo-blast.cfg -w '%{http_code} %{time_total}\n' "$@" >target/demo-blast.txt || true
    t1=$(date +%s%N)
    sort -n -k2 target/demo-blast.txt | awk -v ms=$(((t1 - t0) / 1000000)) '{n++; if ($1 == 200) ok++; t[n] = $2 * 1000}
        END {printf "  %d requests in %d ms: %d/s, p50 %.1f ms, p99 %.1f ms, %d ok\n",
            n, ms, n * 1000 / (ms ? ms : 1), t[int(n * 0.5) + 1], t[int(n * 0.99) + 1], ok}'
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
    until curl -sf "$URL/_nanovm/stats" >/dev/null 2>&1; do
        kill -0 $SERVER 2>/dev/null || { cat "$LOG"; exit 1; }
        sleep 0.1
    done
fi
echo "  stats: $URL/_nanovm/stats, events: $URL/_nanovm/events"

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
echo "  egress to example.com (allowlisted): $(curl -s "$URL/hostile/demo/fetch?url=http://example.com/" | jq -c '{status, bytes, denied}')"
echo "  egress to 169.254.169.254: $(curl -s "$URL/hostile/demo/fetch?url=http://169.254.169.254/latest/meta-data/" | jq -c .)"

say "cold-starting 5,000 new objects"
before=$(curl -s "$URL/_nanovm/stats" | jq .rss_mb)
blast 5000 64 5000 "counter/s$$-%d/"
curl -s "$URL/_nanovm/stats" | jq -r --argjson b "$before" \
    '"  \(.live_objects) live objects, RSS \(.rss_mb | floor) MB, \((.rss_mb - $b) * 1024 / 5000 | floor) KB per new object"'

say "hammering 1,000 warm objects (50,000 requests)"
blast 50000 64 1000 "counter/s$$-%d/"

say "writing /data on 1,000 objects (10,000 POST /incr)"
blast 10000 64 1000 "counter/s$$-%d/incr" -X POST

say "one object serves one request at a time (5,000 requests, 1 client)"
blast 5000 1 1 "counter/solo/"

say "attack storm: 10 rounds of every attack in the hostile guest, all at once"
ATTACKS='/alloc?mb=1024 /spin /sleep?ms=60000 /panic /leak?n=1000000 /fill?mount=tmp&mb=1024 /files?mount=tmp
/write-app /escape /fetch?url=http://169.254.169.254/latest/meta-data/ /fetch?url=http://localtest.me:8080/_nanovm/stats'
: >target/demo-storm.cfg
for r in 1 2 3 4 5 6 7 8 9 10; do
    k=0
    for a in $ATTACKS; do
        k=$((k + 1))
        printf 'url = "%s/hostile/storm-%d-%d%s"\noutput = "/dev/null"\n' "$URL" "$k" "$r" "$a" >>target/demo-storm.cfg
    done
done
t0=$(date +%s%N)
curl -s -Z --parallel-max 110 -K target/demo-storm.cfg \
    -w '%{url} %{http_code} %header{x-nanovm-error} %{time_total}\n' >target/demo-storm.txt || true
echo "  $(wc -l <target/demo-storm.txt) attacks in $((($(date +%s%N) - t0) / 1000000)) ms"
sed 's|^[^ ]*/hostile/storm-[0-9]*-[0-9]*||' target/demo-storm.txt | awk '
    { attack = $1; code = $2; why = (NF == 4) ? $3 : "refused"; t = $NF * 1000
      key = attack; out[key] = code " " why; if (t > slow[key]) slow[key] = t; n[key]++ }
    END { for (k in out) printf "  %-46s %d× %s, slowest %d ms\n", substr(k, 1, 46), n[k], out[k], slow[k] }' | sort

kill $PINGER 2>/dev/null
wait $PINGER 2>/dev/null || true
say "the canary, pinged throughout"
sort -n -k2 "$CANARY" | awk '{n++; if ($1 != 200) bad++; t[n] = $2 * 1000}
    END {printf "  %d requests over HTTP, %d not 200, p50 %.1f ms, p99 %.1f ms (curl per request)\n",
        n, bad, t[int(n * 0.5) + 1], t[int(n * 0.99) + 1]}'
curl -s "$URL/_nanovm/stats" | jq -r '"  node: \(.live_objects) live objects, \(.instantiate.count) instantiations, \(.poisons | to_entries | map(select(.value > 0) | "\(.key) \(.value)") | join(", ")) killed"'

if [ -n "$SERVER" ]; then
    say "node still running on $URL (Ctrl-C to stop)"
    trap 'kill $SERVER 2>/dev/null; exit' INT TERM EXIT
    wait $SERVER
fi
