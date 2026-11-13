#!/bin/bash
# Installs a node and proxy on this systemd host from a nanovm binary:
#   sudo deploy/single-node/install.sh path/to/nanovm [--cgroup]
# Copies the binary, the units, the sysctl drop-in and (only if absent) the
# configs, and makes the secrets directory; with --cgroup also the cgroup
# drop-in. Refuses to replace the binary under a running node
# (docs/ops/runbooks.md, Upgrade). Starts nothing.
set -euo pipefail
[ $# -ge 1 ] || { echo "usage: $0 <nanovm binary> [--cgroup]" >&2; exit 2; }
bin=$1 cgroup=${2:-}
here=$(cd "$(dirname "$0")" && pwd)
units=$here/../systemd
[ "$(id -u)" = 0 ] || { echo "run as root" >&2; exit 1; }
if systemctl is-active -q nanovm; then
    echo "nanovm is running: stop it before replacing its binary (runbooks: Upgrade)" >&2; exit 1
fi
usage=$("$bin" 2>&1 || true)
[[ $usage == *"usage: nanovm serve"* ]] || { echo "$bin is not a nanovm binary" >&2; exit 1; }

if [ -e /usr/local/bin/nanovm ]; then cp -p /usr/local/bin/nanovm /usr/local/bin/nanovm.prev; fi
install -m755 "$bin" /usr/local/bin/nanovm.new && mv /usr/local/bin/nanovm.new /usr/local/bin/nanovm
install -m644 "$units/nanovm.service" "$units/nanovm-proxy.service" /etc/systemd/system/
install -d /etc/nanovm
# Apps' secrets (nanovm.toml's `secrets`): root's alone, or the node won't start.
install -d -m700 /etc/nanovm/secrets
for f in nanovm.toml proxy.toml; do
    if [ ! -e "/etc/nanovm/$f" ]; then install -m644 "$units/$f" /etc/nanovm/; else echo "kept /etc/nanovm/$f"; fi
done
install -m644 "$units/90-nanovm.conf" /etc/sysctl.d/
sysctl -q -p /etc/sysctl.d/90-nanovm.conf || echo "could not apply vm.max_map_count now (read-only /proc/sys?)"
if [ "$cgroup" = --cgroup ]; then
    install -d /etc/systemd/system/nanovm.service.d
    install -m644 "$units/nanovm-cgroup.conf" /etc/systemd/system/nanovm.service.d/cgroup.conf
    grep -q '^cgroup *=' /etc/nanovm/nanovm.toml ||
        sed -i '1i cgroup = "/sys/fs/cgroup/system.slice/nanovm.service/apps"' /etc/nanovm/nanovm.toml
fi
systemctl daemon-reload
python3 "$here/check-host.py" /etc/nanovm/nanovm.toml --nofile "$(systemctl show -p LimitNOFILE --value nanovm)"
echo "next: systemctl enable --now nanovm nanovm-proxy"
