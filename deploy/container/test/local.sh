#!/bin/bash
# Validates the image's runtime stage and the documented `docker run` flags
# without pulling any image: a stand-in base is imported from this host's
# glibc and busybox, and a locally built dev binary replaces the build stage
# (BuildKit named context `build`). It does not test the build stage or the
# real base images. Testing only. Needs a running dockerd (DOCKER_HOST), root,
# busybox, and a dev build:
#
#   cargo build -p nanovm --features dev --bin nanovm --example dev-registry
#   deploy/container/test/local.sh          # BIN=target/debug by default
set -u
REPO=$(cd "$(dirname "$0")/../../.." && pwd)
BIN=${BIN:-$REPO/target/debug}
WORK=$(mktemp -d "${TMPDIR:-/tmp}/nanovm-image.XXXXXX")
fail=0
check() { if eval "$2"; then echo "PASS $1"; else echo "FAIL $1"; fail=1; fi; }
cleanup() {
    docker rm -f nv-node nv-reg nv-proxy >/dev/null 2>&1
    docker volume rm -f nv-data >/dev/null 2>&1
    rm -rf "$WORK"
}
trap cleanup EXIT
cleanup; mkdir -p "$WORK"

# Stand-in for debian:trixie-slim: the binary's libraries, busybox, /tmp.
B=$WORK/base
mkdir -p "$B/bin" "$B/tmp" "$B/etc" && chmod 1777 "$B/tmp"
for f in $(ldd "$BIN/nanovm" | grep -o '/lib[^ ]*' | sort -u); do
    mkdir -p "$B$(dirname "$f")" && cp -L "$f" "$B$f"
done
cp "$(command -v busybox)" "$B/bin/" && for c in sh ps cat ls wget sleep grep id; do ln -s busybox "$B/bin/$c"; done
echo 'root:x:0:0:root:/root:/bin/sh' >"$B/etc/passwd"
tar -C "$B" -c . | docker import - nanovm-standin:local >/dev/null

# The build stage's outputs, from this host.
C=$WORK/build
install -D "$BIN/nanovm" "$C/out/nanovm"
install -D /etc/ssl/certs/ca-certificates.crt "$C/etc/ssl/certs/ca-certificates.crt"
docker buildx build -q --load -f "$REPO/deploy/container/Dockerfile" --build-context build="$C" \
    --build-arg RUNTIME_IMAGE=nanovm-standin:local -t nanovm:local-test "$REPO" >/dev/null || exit 1
echo "image: $(docker image inspect -f '{{.Size}}' nanovm:local-test) bytes, $(docker history -q nanovm:local-test | wc -l) layers"

# Dev mode for unsigned apps from a loopback registry; otherwise the image's config.
sed '1i dev = true' "$REPO/deploy/container/nanovm.toml" >"$WORK/nanovm.toml"
sed '1i dev = true' "$REPO/deploy/container/proxy.toml" | sed 's/^listen = "0.0.0.0:8443"/listen = "127.0.0.1:8443"/' >"$WORK/proxy.toml"
# NOFILE: this sandbox cannot raise its hard limit past 20000.
NODE_FLAGS=(--security-opt no-new-privileges --ulimit "nofile=${NOFILE:-1048576}:${NOFILE:-1048576}" --cap-drop ALL --cap-add CHOWN --cap-add SETUID
    --cap-add SETGID --cap-add KILL -v nv-data:/var/lib/nanovm -v "$WORK/nanovm.toml:/etc/nanovm/nanovm.toml:ro")
node() { docker run -d --name nv-node --network none "$@" "${NODE_FLAGS[@]}" nanovm:local-test >/dev/null; }
x() { docker exec nv-node "$@"; }
ready() { for _ in $(seq 50); do x wget -q -O /dev/null http://127.0.0.1:8080/_nanovm/ready 2>/dev/null && return 0; sleep 0.2; done; return 1; }
req() { # METHOD APP PATH
    local data=(); [ "$1" = POST ] && data=(--post-data '')
    x wget -S -O - "${data[@]}" --header "nanovm-app: $(awk -v a="$2" '$1 == a {print $2}' "$WORK/apps")" \
        --header 'nanovm-trust: dev dev' "http://127.0.0.1:8080$3" 2>&1
}

registry() {
    docker run -d --name nv-reg --network container:nv-node -v "$REPO:$REPO:ro" -v "$BIN:$BIN:ro" \
        --entrypoint "$BIN/examples/dev-registry" nanovm:local-test >/dev/null
    for _ in $(seq 50); do docker logs nv-reg 2>/dev/null >"$WORK/apps"; [ "$(grep -c ' 127' "$WORK/apps")" = 2 ] && break; sleep 0.2; done
}

node --init
registry
check "node ready (--init)" ready
for _ in 1 2 3; do req POST counter /alice/incr >/dev/null; done
check "counter at 3" 'req GET counter /alice/ | grep -q "\"count\":3"'
W=$(x sh -c 'for p in /proc/[0-9]*; do grep -q "nanovm.worker" $p/cmdline 2>/dev/null && basename $p; done' | head -1)
echo "worker: $(x grep -E '^(Uid|NoNewPrivs|Seccomp):' "/proc/$W/status" | tr -s '\t\n' '  ')"
check "worker runs as an app uid" '[ "$(x grep ^Uid: /proc/$W/status | cut -f2)" -ge 200000 ]'
check "worker seccomp and no_new_privs" 'x grep -q "Seccomp:.2" /proc/$W/status && x grep -q "NoNewPrivs:.1" /proc/$W/status'
check "hostile spin killed by its CPU budget (500)" 'req GET hostile /h/spin | grep -q "HTTP/1.1 500"'
check "hostile escapes refused (403)" 'req GET hostile /h/escape | grep -q "HTTP/1.1 403"'
docker rm -f nv-node nv-reg >/dev/null
node --init
registry
ready >/dev/null
check "volume survives a new container" 'req GET counter /alice/ | grep -q "\"count\":3"'

docker run -d --name nv-proxy --network container:nv-node --init --security-opt no-new-privileges --cap-drop ALL --user 65534:65534 --read-only \
    -v "$WORK/proxy.toml:/etc/nanovm/proxy.toml:ro" nanovm:local-test proxy --config /etc/nanovm/proxy.toml >/dev/null
sleep 1
check "proxy answers as nobody, read-only (502: no resolver here)" \
    'x wget -S -O - --header "Host: app.example.com" http://127.0.0.1:8443/alice/ 2>&1 | grep -q "HTTP/1.1 502"'
t0=$(date +%s%N); docker stop nv-node >/dev/null; t1=$(date +%s%N)
echo "docker stop with --init: $(((t1 - t0) / 1000000)) ms"
check "stop with --init takes under 5 s" '[ $(((t1 - t0) / 1000000)) -lt 5000 ]'
docker rm -f nv-node nv-reg nv-proxy >/dev/null 2>&1

# Without --init the supervisor is PID 1: workers must still start; SIGTERM
# is ignored by a PID 1 without a handler, so stop waits out its timeout.
node
registry
ready >/dev/null
check "as PID 1, workers start" 'req POST counter /bob/incr | grep -q "\"count\":1"'
docker logs nv-node 2>&1 | grep -m1 'nanovm\[' | sed 's/^/  worker said: /'
t0=$(date +%s%N); docker stop -t 3 nv-node >/dev/null; t1=$(date +%s%N)
echo "docker stop -t 3 as PID 1: $(((t1 - t0) / 1000000)) ms"
echo "RESULT fail=$fail"
exit $fail
