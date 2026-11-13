#!/bin/bash
# Runs inside a throwaway systemd container (nspawn.sh): installs the units,
# starts the node and proxy, drives the counter and hostile apps, restarts,
# stops, and reports. Dev mode (dev-registry on loopback, unsigned apps).
set -u
D=$(cd "$(dirname "$0")/.." && pwd)
fail=0
check() { if eval "$2"; then echo "PASS $1"; else echo "FAIL $1"; fail=1; fi; }
systemctl stop nanovm nanovm-proxy devreg 2>/dev/null
systemctl reset-failed 2>/dev/null
rm -rf /var/lib/nanovm /var/lib/private/nanovm /etc/nanovm /etc/systemd/system/nanovm.service.d /srv/nanovm-store
install -m644 $D/nanovm.service $D/nanovm-proxy.service /etc/systemd/system/
install -d /etc/nanovm && install -m644 $D/nanovm.toml $D/proxy.toml /etc/nanovm/ && install -d -m700 /etc/nanovm/secrets
install -m644 $D/90-nanovm.conf /etc/sysctl.d/
# Unsigned apps from the loopback registry: dev mode (dev build, loopback).
sed -i '1i dev = true' /etc/nanovm/nanovm.toml
sed -i '1i dev = true' /etc/nanovm/proxy.toml
if [ -n "${CGROUP:-}" ]; then
  install -d /etc/systemd/system/nanovm.service.d
  cp $D/nanovm-cgroup.conf /etc/systemd/system/nanovm.service.d/cgroup.conf
  sed -i '1i cgroup = "/sys/fs/cgroup/system.slice/nanovm.service/apps"' /etc/nanovm/nanovm.toml
fi
# Local capture publishes versions with link(2): the unit's filter must allow it.
[ -n "${CAPTURE:-}" ] && printf '\n[capture]\nmax_mb = 64\n' >>/etc/nanovm/nanovm.toml
# Capture shipped to a directory store, through the store drop-in.
if [ -n "${STORE:-}" ]; then
  install -d /etc/systemd/system/nanovm.service.d /srv/nanovm-store
  cp $D/nanovm-store.conf /etc/systemd/system/nanovm.service.d/store.conf
  printf '\n[capture]\nstore = "/srv/nanovm-store"\n' >>/etc/nanovm/nanovm.toml
fi
systemctl daemon-reload
rm -f /run/devreg.out
systemd-run -q --unit=devreg -p StandardOutput=file:/run/devreg.out /usr/local/bin/dev-registry
for _ in $(seq 50); do [ "$(grep -c "^[a-z]* 127" /run/devreg.out 2>/dev/null)" = 2 ] && break; sleep 0.2; done
for app in counter hostile; do
  printf 'nanovm-app: %s\nnanovm-trust: dev dev\n' "$(awk -v a=$app '$1 == a {print $2}' /run/devreg.out)" > /run/$app.h
done
systemctl start nanovm
for _ in $(seq 50); do curl -sf http://127.0.0.1:8080/_nanovm/ready >/dev/null && break; sleep 0.2; done
check "node active" '[ "$(systemctl is-active nanovm)" = active ]'
check "ready 204" '[ "$(curl -s -o /dev/null -w "%{http_code}" http://127.0.0.1:8080/_nanovm/ready)" = 204 ]'
for _ in 1 2 3; do curl -s -XPOST -H @/run/counter.h http://127.0.0.1:8080/alice/incr; echo; done
check "counter at 3" 'curl -s -H @/run/counter.h http://127.0.0.1:8080/alice/ | grep -q "\"count\":3"'
if [ -n "${CAPTURE:-}" ]; then
  echo "captured versions of alice: $(find /var/lib/nanovm/objects/*/*/alice/committed -type f | wc -l)"
  check "three captured versions" '[ "$(find /var/lib/nanovm/objects/*/*/alice/committed -type f -name "*.tar" | wc -l)" = 3 ]'
fi
if [ -n "${STORE:-}" ]; then
  for _ in $(seq 50); do grep -qs '"sequence":3' /srv/nanovm-store/*/alice/head && break; sleep 0.2; done
  echo "store: $(cd /srv/nanovm-store && find . -path '*alice*' -type f | sort | tr '\n' ' ')"
  check "three versions shipped, the head names the third" '[ "$(find /srv/nanovm-store/*/alice -name "*.tar" | wc -l)" = 3 ] && grep -q "\"sequence\":3" /srv/nanovm-store/*/alice/head'
  check "shipped versions removed here but the last" '[ "$(find /var/lib/nanovm/objects/*/*/alice/committed -name "*.tar" | wc -l)" = 1 ]'
fi
W=$(pgrep -f "nanovm worker" | head -1)
echo "worker pid $W uid $(ps -o uid= -p $W) caps $(grep CapEff /proc/$W/status)"
check "worker runs as an app uid" '[ "$(ps -o uid= -p $W | tr -d " ")" -ge 200000 ]'
check "worker seccomp on" 'grep -q "Seccomp:[[:space:]]*2" /proc/$W/status'
check "worker no_new_privs" 'grep -q "NoNewPrivs:[[:space:]]*1" /proc/$W/status'
echo "supervisor: $(grep -E 'CapBnd|CapEff|NoNewPrivs|Seccomp:' "/proc/$(systemctl show -p MainPID --value nanovm)/status" | tr '\n' ' ')"
ls -ln /var/lib/nanovm /var/lib/nanovm/objects
code=$(curl -s -o /dev/null -w '%{http_code}' -H @/run/hostile.h http://127.0.0.1:8080/h1/spin)
check "hostile spin killed (500 cpu)" '[ "$code" = 500 ]'
code=$(curl -s -o /run/esc.json -w '%{http_code}' -H @/run/hostile.h "http://127.0.0.1:8080/h1/escape")
echo "escape: $code $(head -c 300 /run/esc.json)"
# One worker per app and the spare the supervisor keeps ready (N-224).
check "three workers running (two apps, one spare)" '[ "$(pgrep -fc "nanovm worker")" = 3 ]'
if [ -n "${CGROUP:-}" ]; then
  G=/sys/fs/cgroup/system.slice/nanovm.service
  echo "cgroups: $(cd $G && find . -name cgroup.procs | sort | while read f; do echo "$(dirname $f)=[$(tr '\n' ' ' < $f)]"; done | tr '\n' ' ')"
  check "supervisor in supervisor/" 'grep -qx "$(systemctl show -p MainPID --value nanovm)" $G/supervisor/cgroup.procs'
  check "each worker in apps/worker-<n>" '[ "$(cat $G/apps/*/cgroup.procs | sort | tr "\n" " ")" = "$(pgrep -f "nanovm worker" | sort | tr "\n" " ")" ]'
  check "service group itself empty" '[ ! -s $G/cgroup.procs ]'
fi
W=$(pgrep -f "nanovm worker" | head -1)
check "kill(2) across uids needs CAP_KILL" '! systemd-run -q --wait -p CapabilityBoundingSet=CAP_CHOWN /bin/kill -0 $W 2>/dev/null && systemd-run -q --wait -p CapabilityBoundingSet=CAP_KILL /bin/kill -0 $W'
systemctl restart nanovm
for _ in $(seq 50); do curl -sf http://127.0.0.1:8080/_nanovm/ready >/dev/null && break; sleep 0.2; done
check "count survives restart" 'curl -s -H @/run/counter.h http://127.0.0.1:8080/alice/ | grep -q "\"count\":3"'
check "uid stable after restart" '[ "$(ps -o uid= -p $(pgrep -f "nanovm worker" | head -1) | tr -d " ")" = 200000 ]'
systemctl start nanovm-proxy
sleep 1
check "proxy active" '[ "$(systemctl is-active nanovm-proxy)" = active ]'
code=$(curl -s -o /run/px.json -w '%{http_code}' -H "Host: app.example.com" http://127.0.0.1:8443/alice/)
echo "proxy: $code $(cat /run/px.json)"
check "proxy answers (no resolver in a private net: 502)" '[ "$code" = 502 ]'
echo "proxy: $(grep -E 'Uid|CapEff|NoNewPrivs|Seccomp:' "/proc/$(systemctl show -p MainPID --value nanovm-proxy)/status" | tr '\n' ' ')"
systemctl stop nanovm nanovm-proxy
check "node stopped" '[ "$(systemctl is-active nanovm)" = inactive ]'
check "no worker left" '! pgrep -f "nanovm worker" >/dev/null'
check "proxy stopped" '[ "$(systemctl is-active nanovm-proxy)" = inactive ]'
systemctl stop devreg
journalctl -u nanovm -u nanovm-proxy --no-pager -n 40 | grep -v 'opentelemetry\|OTLP\|BatchLogProcessor\|BatchSpanProcessor' | tail -20
echo "RESULT fail=$fail"
