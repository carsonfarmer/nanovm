#!/bin/bash
# Replaces the nanovm binary under a running supervisor (runbooks.md,
# "Upgrade"), then starts a new app's worker. OLD and NEW are dev build
# directories (nanovm and examples/dev-registry) of two builds whose
# supervisor-worker spec differs. The new app must start from the running
# image (N-552):
#   docs/ops/drills/binary-swap.sh OLD_DIR NEW_DIR
set -u
OLD=$1 NEW=$2; PORT=28580; REG=25580
W=$(mktemp -d /var/tmp/nanovm-swap.XXXXXX); chmod 755 $W
pids=()
trap 'kill "${pids[@]}" 2>/dev/null; wait 2>/dev/null; rm -rf $W' EXIT
$OLD/examples/dev-registry 127.0.0.1:$REG > $W/apps 2>/dev/null & pids+=($!)
sleep 2
for a in counter hostile; do printf 'nanovm-app: %s\nnanovm-trust: dev dev\n' "$(awk -v a=$a '$1==a{print $2}' $W/apps)" > $W/$a.h; done
install -m755 $OLD/nanovm $W/nanovm
printf 'listen = "127.0.0.1:%s"\ndata_dir = "%s/data"\ndev = true\ntelemetry_endpoint = "http://127.0.0.1:9"\n' $PORT $W > $W/n.toml
env -u RUST_BACKTRACE $W/nanovm serve --config $W/n.toml > $W/n.log 2>&1 & pids+=($!)
sleep 1
echo "before: $(curl -s -XPOST -H @$W/counter.h http://127.0.0.1:$PORT/a/incr)"
cp $NEW/nanovm $W/nanovm 2>&1 | head -1
install -m755 $NEW/nanovm $W/nanovm && echo "replaced with install"
echo "running app: $(curl -s -XPOST -H @$W/counter.h http://127.0.0.1:$PORT/a/incr)"
echo "new app's worker: $(curl -s -H @$W/hostile.h http://127.0.0.1:$PORT/h/ | head -c 300)"
