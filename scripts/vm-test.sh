#!/bin/sh
# Runs integration tests inside a throwaway QEMU VM whose kernel gives
# cgroup v2 every controller, for hosts (like some CI sandboxes) whose
# memory, cpu and pids controllers are held by cgroup v1. Testing only.
#
#   scripts/vm-test.sh [test-name-filter,... | all]   (default: cgroups and the jail)
#
# Without KVM the guest CPU is emulated, many times slower, so tests that
# assert CPU budgets or latencies fail under `all`; the default set does not
# depend on speed.
#
# Needs qemu-system-x86_64 and an Ubuntu-style apt for the guest kernel.
# The VM boots from an initramfs holding busybox, the test binary, nanovm
# and the guests at their paths on this machine.
set -e
cd "$(dirname "$0")/.."
N=$(pwd)
TESTS=${1:-workers_are_placed,worker_memory,worker_cpu,jail_blocks_escapes}
[ "$TESTS" = all ] && TESTS=
V=$N/target/vm
mkdir -p "$V"

if ! ls "$V"/vmlinuz-* >/dev/null 2>&1; then
    pkg=$(apt-cache search --names-only '^linux-image-[0-9.]+-[0-9]+-generic$' | sort -V | tail -1 | cut -d' ' -f1)
    (cd "$V" && apt-get download "$pkg" && dpkg-deb -x linux-image-*.deb k && cp k/boot/vmlinuz-* . && rm -rf k ./*.deb)
fi

cargo test --release -p nanovm --test integration --no-run 2>&1 | tail -1
T=$(ls -t target/release/deps/integration-* | grep -v '\.d$' | head -1)

accel=tcg
[ -w /dev/kvm ] && accel=kvm
R=$V/root
rm -rf "$R"
mkdir -p "$R/bin" "$R/proc" "$R/sys" "$R/dev" "$R/tmp" "$R$N/crates/nanovm" "$R$N/target/release/deps" \
    "$R$N/target/tmp" "$R$N/guests/target/wasm32-wasip2/release" "$R$N/demo"
cp "$(command -v busybox)" "$R/bin/"
for c in sh mount ip poweroff sysctl cat cut sed tr; do ln -s busybox "$R/bin/$c"; done
cp "$T" "$R$N/target/release/deps/integration"
cp target/release/nanovm "$R$N/target/release/"
cp guests/target/wasm32-wasip2/release/counter.wasm guests/target/wasm32-wasip2/release/hostile.wasm \
    "$R$N/guests/target/wasm32-wasip2/release/"
cp -r demo/static "$R$N/demo/"
mkdir -p "$R/etc" && echo "127.0.0.1 localhost" >"$R/etc/hosts"
for f in $(ldd "$T" target/release/nanovm | grep -o '/lib[^ ]*' | sort -u); do
    mkdir -p "$R$(dirname "$f")"
    cp -L "$f" "$R$f"
done
cat >"$R/init" <<INIT
#!/bin/sh
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
mount -t cgroup2 none /sys/fs/cgroup
echo "+memory +cpu +pids" > /sys/fs/cgroup/cgroup.subtree_control
ip link set lo up
sysctl -qw vm.max_map_count=1048576
echo "== kernel \$(cut -d' ' -f3 /proc/version), cgroup v2: \$(cat /sys/fs/cgroup/cgroup.controllers)"
cd $N
$N/target/release/deps/integration \$(echo "$TESTS" | tr , ' ') --test-threads 1 2>&1
echo "== exit \$?"
poweroff -f
INIT
chmod +x "$R/init"
(cd "$R" && find . | busybox cpio -o -H newc 2>/dev/null | gzip -1 >"$V/initrd.gz")

qemu-system-x86_64 -accel $accel -cpu max -smp 4 -m 3072 -nographic -no-reboot \
    -kernel "$(ls "$V"/vmlinuz-* | head -1)" -initrd "$V/initrd.gz" \
    -append "console=ttyS0 quiet panic=-1" 2>&1 | tr -d '\r' | sed -n '/== kernel/,$p' | tee "$V/out.txt"
grep -q '^== exit 0' "$V/out.txt"
