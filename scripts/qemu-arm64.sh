#!/bin/bash
# Boot the arm64 kernel on QEMU virt. Uses HVF acceleration on Apple
# Silicon (set NOS_TCG=1 to force TCG); serial console goes to nos-arm64.log.
cd "$(dirname "$0")/.."

ACCEL_OPTS="-accel tcg -cpu cortex-a72"
if [ "$(uname -sm)" = "Darwin arm64" ] && [ -z "$NOS_TCG" ]; then
    ACCEL_OPTS="-accel hvf -cpu host"
fi

qemu-system-aarch64 \
    -M virt,gic-version=3 \
    -smp 2 \
    -m 1024 \
    $ACCEL_OPTS \
    -kernel nos-arm64.img \
    -global virtio-mmio.force-legacy=false \
    -serial file:nos-arm64.log \
    -display none \
    "$@"
