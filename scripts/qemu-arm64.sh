#!/bin/bash
# Boot the arm64 kernel on QEMU virt with virtio-mmio disk/net/rng.
# Uses HVF acceleration on Apple Silicon (set NOS_TCG=1 to force TCG);
# serial console goes to nos-arm64.log, UDP shell forwarded on :9000.
cd "$(dirname "$0")/.."

ACCEL_OPTS="-accel tcg -cpu cortex-a72"
if [ "$(uname -sm)" = "Darwin arm64" ] && [ -z "$NOS_TCG" ]; then
    ACCEL_OPTS="-accel hvf -cpu host"
fi

[ -f nos-arm64.qcow2 ] || qemu-img create -f qcow2 nos-arm64.qcow2 256M

# exec so this script's PID is qemu's PID (kill-able by callers)
exec qemu-system-aarch64 \
    -M virt,gic-version=3 \
    -smp 4 \
    -m 1024 \
    $ACCEL_OPTS \
    -kernel nos-arm64.img \
    -append "dhcp=auto dns=on udpshell=9000" \
    -global virtio-mmio.force-legacy=false \
    -drive file=nos-arm64.qcow2,format=qcow2,id=hd,if=none \
    -device virtio-blk-device,drive=hd \
    -device virtio-net-device,netdev=net0 \
    -netdev user,id=net0,hostfwd=udp::9000-:9000 \
    -device virtio-rng-device \
    -serial file:nos-arm64.log \
    -display none \
    "$@"
