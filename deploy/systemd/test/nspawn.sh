#!/bin/bash
# Boots this host's own OS (its /usr under a throwaway overlay, a copy of its
# /etc) in systemd-nspawn, and runs in-container.sh there against the units in
# deploy/systemd: plain, with the cgroup drop-in on a cgroup v2 hierarchy, and
# with local capture on. Testing only. Needs root, systemd-nspawn (apt: systemd-container)
# and a dev build of the node and the loopback registry:
#
#   cargo build -p nanovm --features dev --bin nanovm --example dev-registry
#   sudo deploy/systemd/test/nspawn.sh            # BIN=target/debug by default
set -euo pipefail
REPO=$(cd "$(dirname "$0")/../../.." && pwd)
BIN=${BIN:-$REPO/target/debug}
WORK=$(mktemp -d "${TMPDIR:-/tmp}/nanovm-nspawn.XXXXXX")
[ -n "${KEEP:-}" ] || trap 'rm -rf "$WORK"' EXIT
R=$WORK/root U=$WORK/usr
mkdir -p "$R"/{usr,dev,proc,sys,run,tmp,var/tmp,var/lib,root} "$U/local/bin"
for d in bin lib lib64 sbin; do ln -s "usr/$d" "$R/$d"; done
cp -a /etc "$R/etc"
rm -f "$R/etc/os-release" && cp /usr/lib/os-release "$R/etc/os-release"
: >"$R/etc/machine-id"
# Start nothing of the host's but the test.
rm -rf "$R"/etc/systemd/system/{multi-user,sockets,timers,getty}.target.wants
cp "$BIN/nanovm" "$BIN/examples/dev-registry" "$U/local/bin/"
mkdir -p "$R/etc/systemd/system/multi-user.target.wants"
cat >"$R/etc/systemd/system/nanovm-test.service" <<EOF
[Unit]
After=multi-user.target
[Service]
Type=oneshot
EnvironmentFile=-/etc/nanovm-test.env
ExecStart=/bin/bash $REPO/deploy/systemd/test/in-container.sh
StandardOutput=journal+console
ExecStopPost=/bin/systemctl poweroff --no-block
EOF
ln -s ../nanovm-test.service "$R/etc/systemd/system/multi-user.target.wants/"

status=0
for mode in plain cgroup capture; do
    echo "== $mode"
    unified=0
    rm -f "$R/etc/nanovm-test.env"
    if [ $mode = cgroup ]; then unified=1; echo CGROUP=1 >"$R/etc/nanovm-test.env"; fi
    if [ $mode = capture ]; then echo CAPTURE=1 >"$R/etc/nanovm-test.env"; fi
    # The repository read-only at its own path: dev-registry finds the guests
    # there. nspawn's own seccomp filter lacks Landlock, which workers need.
    SYSTEMD_NSPAWN_UNIFIED_HIERARCHY=$unified timeout 600 systemd-nspawn -q -D "$R" \
        --overlay="/usr:$U:/usr" --bind-ro="$REPO" --private-network --register=no \
        --system-call-filter="landlock_create_ruleset landlock_add_rule landlock_restrict_self" \
        --keep-unit -M nanovm-test --boot >"$WORK/$mode.log" 2>&1 || true
    tr -d '\r' <"$WORK/$mode.log" | sed -n 's/^.*bash\[[0-9]*\]: //p' >"$WORK/$mode.out"
    grep -aE '^(PASS|FAIL|RESULT|cgroups:|supervisor:|proxy:|worker pid|captured)' "$WORK/$mode.out" | sed 's/^/  /'
    grep -aqx 'RESULT fail=0' "$WORK/$mode.out" || status=1
done
exit $status
