#!/bin/bash
# Triggers each x-nanovm-error reason it can against a dev-mode node and
# prints status and reason, for docs/ops/errors.md. Needs a dev build:
#   cargo build -p nanovm --features dev --bin nanovm --example dev-registry
# Ports: node 127.0.0.1:${PORT:-28080}, registry 127.0.0.1:${REG:-25000}.
set -u
REPO=$(cd "$(dirname "$0")/../.." && pwd)
BIN=${BIN:-$REPO/target/debug}
PORT=${PORT:-28080} REG=${REG:-25000}
# Workers run as other uids when root, so data_dir must be traversable.
W=$(mktemp -d /var/tmp/nanovm-errors.XXXXXX) && chmod 755 "$W"
pids=()
trap 'kill "${pids[@]}" 2>/dev/null; wait 2>/dev/null; rm -rf "$W"' EXIT
U=http://127.0.0.1:$PORT

"$BIN/examples/dev-registry" "127.0.0.1:$REG" >"$W/apps" 2>"$W/reg.log" & pids+=($!)
for _ in $(seq 100); do [ "$(grep -c " 127" "$W/apps")" = 2 ] && break; sleep 0.1; done
hdr() { printf 'nanovm-app: %s\nnanovm-trust: dev dev\n' "$(awk -v a="$1" '$1 == a {print $2}' "$W/apps")" >"$W/$1.h"; }
hdr counter; hdr hostile

node() { # NAME [extra config lines...]
    local name=$1; shift
    { echo 'dev = true'; echo "listen = \"127.0.0.1:$PORT\""; echo "data_dir = \"$W/$name\""
      echo 'telemetry_endpoint = "http://127.0.0.1:9"'; printf '%s\n' "$@"; } >"$W/$name.toml"
    env -u RUST_BACKTRACE "$BIN/nanovm" serve --config "$W/$name.toml" >"$W/$name.log" 2>&1 & pids+=($!)
    for _ in $(seq 100); do curl -sf "$U/_nanovm/ready" >/dev/null && return; sleep 0.1; done
}
stop() { kill "${pids[-1]}"; wait "${pids[-1]}" 2>/dev/null; unset 'pids[-1]'; }
# show LABEL curl-args...: status, x-nanovm-error, and the start of the detail
show() {
    local label=$1; shift
    curl -s -o "$W/body" -D "$W/head" "$@"
    printf '%-34s %s %-20s %s\n' "$label" "$(head -1 "$W/head" | cut -d' ' -f2)" \
        "$(grep -i '^x-nanovm-error:' "$W/head" | cut -d' ' -f2 | tr -d '\r')" "$(head -c 110 "$W/body" | tr '\n' ' ')"
}

node main 'max_live_objects = 3' '[limits]' 'max_queue = 1' 'max_request_body_kb = 64'
C=(-H @"$W/counter.h") H=(-H @"$W/hostile.h")
show "no headers" "$U/x/"
show "unknown node route" "$U/_nanovm/nope"
show "garbage nanovm-app" -H 'nanovm-app: nope' -H 'nanovm-trust: dev dev' "$U/x/"
show "object id starting with _" "${C[@]}" "$U/_x/"
show "body over max_request_body_kb" "${C[@]}" --data-binary @<(head -c 100000 /dev/zero) "$U/x/incr"
show "guest returns an error code" "${H[@]}" "$U/h1/commit?then=error"
show "guest panics" "${H[@]}" "$U/h1/panic"
show "spins past cpu_ms" "${H[@]}" "$U/h1/spin"
show "allocates past memory_mb" "${H[@]}" "$U/h1/alloc?mb=40"
show "sleeps past request_timeout_ms" "${H[@]}" "$U/h1/sleep?ms=5000"
show "leaks handles past max_resources" "${H[@]}" "$U/h1/leak"
show "calls itself (inner status)" "${H[@]}" "$U/h1/fetch?url=http://nanovm/objects/h1/"
# Two requests ahead of a third on one object, queue of 1: the third is refused.
bg=()
for _ in 1 2; do curl -s -o /dev/null "${H[@]}" "$U/q/sleep?ms=1000" & bg+=($!); done; sleep 0.3
show "queue full (max_queue = 1)" "${H[@]}" "$U/q/sleep?ms=10"
wait "${bg[@]}"
# Three busy objects fill max_live_objects = 3; a fourth finds none idle.
bg=()
for o in b1 b2 b3; do curl -s -o /dev/null "${H[@]}" "$U/$o/sleep?ms=1500" & bg+=($!); done; sleep 0.5
show "no live slot (max_live_objects = 3)" "${H[@]}" "$U/b4/"
wait "${bg[@]}"
show "digest the registry lacks" -H "nanovm-app: 127.0.0.1:$REG/nanovm/counter@sha256:$(printf '0%.0s' $(seq 64))" \
    -H 'nanovm-trust: dev dev' "$U/x/"
stop

node tiny '[limits]' 'memory_mb = 1'
show "memory_mb below the guest's start" -H @"$W/counter.h" "$U/x/"

stop

# Local capture: history full, then capture turned off on a root with history.
node cap '[capture]' 'max_versions = 2'
for _ in 1 2; do curl -s -o /dev/null -XPOST -H @"$W/counter.h" "$U/c/incr"; done
show "capture history full (max_versions = 2)" -XPOST -H @"$W/counter.h" "$U/c/incr"
stop
node cap
show "capture turned off, history kept" -H @"$W/counter.h" "$U/c/"

# The proxy, with a resolver that isn't there.
P=http://127.0.0.1:$((PORT + 1))
printf 'dev = true\nlisten = "%s"\nnode = "127.0.0.1:%s"\npublish_listen = "127.0.0.1:%s"\nresolver = "http://127.0.0.1:9/dns-query"\ntelemetry_endpoint = "http://127.0.0.1:9"\n' \
    "${P#http://}" "$PORT" "$((PORT + 2))" >"$W/proxy.toml"
"$BIN/nanovm" proxy --config "$W/proxy.toml" >"$W/proxy.log" 2>&1 & pids+=($!)
for _ in $(seq 50); do curl -s -o /dev/null "$P/" && break; sleep 0.1; done
show "proxy: no DNS answer" -H 'Host: example.com' "$P/x/"
show "proxy: client-sent socket event" -H 'Host: example.com' -H 'content-type: application/websocket-events' "$P/x/"
