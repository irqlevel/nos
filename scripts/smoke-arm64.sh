#!/bin/bash
# Boot smoke test for the arm64 kernel: build (in Docker), boot QEMU virt
# headless with virtio-mmio blk/net/rng attached, and assert the serial log
# reaches the boot-success markers. Uses TCG by default so it runs anywhere
# (CI included); set SMOKE_HVF=1 on Apple Silicon for the accelerated run.
#
# Usage: scripts/smoke-arm64.sh [--skip-build]
# Env:   SMOKE_TIMEOUT  seconds to wait for boot markers (default 300)
#        SMOKE_HVF      set to 1 to use -accel hvf -cpu host
#        SMOKE_LOG      serial log path (default <tmpdir>/nos-arm64.log)
set -u
cd "$(dirname "$0")/.."

SKIP_BUILD=0
[ "${1:-}" = "--skip-build" ] && SKIP_BUILD=1

SMOKE_TIMEOUT="${SMOKE_TIMEOUT:-300}"

ACCEL_OPTS="-accel tcg -cpu cortex-a72"
[ "${SMOKE_HVF:-0}" = "1" ] && ACCEL_OPTS="-accel hvf -cpu host"

TMPDIR_SMOKE="$(mktemp -d)"
SMOKE_LOG="${SMOKE_LOG:-$TMPDIR_SMOKE/nos-arm64.log}"
QEMU_PID=""

cleanup() {
    [ -n "$QEMU_PID" ] && kill "$QEMU_PID" 2>/dev/null
    rm -rf "$TMPDIR_SMOKE"
}
trap cleanup EXIT

fail() {
    echo "SMOKE-ARM64 FAIL: $1"
    if [ -f "$SMOKE_LOG" ]; then
        echo "--- last 30 lines of serial log ---"
        tail -n 30 "$SMOKE_LOG"
    fi
    exit 1
}

if [ "$SKIP_BUILD" = "0" ]; then
    echo "smoke-arm64: building (docker, incremental)..."
    docker run --platform linux/amd64 --rm -v "$PWD:/src" -w /src nos-builder \
        bash -c 'make nocheck ARCH=aarch64' > "$TMPDIR_SMOKE/build.log" 2>&1 \
        || { tail -n 30 "$TMPDIR_SMOKE/build.log"; fail "build failed"; }
fi

[ -f nos-arm64.img ] || fail "nos-arm64.img not found"

qemu-img create -q -f qcow2 "$TMPDIR_SMOKE/blk0.qcow2" 256M

echo "smoke-arm64: booting (log: $SMOKE_LOG, timeout: ${SMOKE_TIMEOUT}s)..."
qemu-system-aarch64 \
    -M virt,gic-version=3 \
    -smp 4 \
    -m 1024 \
    $ACCEL_OPTS \
    -kernel nos-arm64.img \
    -global virtio-mmio.force-legacy=false \
    -drive "file=$TMPDIR_SMOKE/blk0.qcow2,format=qcow2,id=hd,if=none" \
    -device virtio-blk-device,drive=hd \
    -device virtio-net-device,netdev=net0 -netdev user,id=net0 \
    -device virtio-rng-device \
    -serial "file:$SMOKE_LOG" \
    -display none \
    &
QEMU_PID=$!

# Printed in this order on a healthy boot:
#   "Self test passed"   - Test::Test() green (mm/lib self-tests)
#   "Cpus started"       - every DTB CPU came up via PSCI CPU_ON
#   "After test"         - multitasking test green with SMP on
#   "boot: complete"     - shell started, BSP reached the idle loop
MARKERS=("Self test passed" "Cpus started" "After test" "boot: complete")

ELAPSED=0
while [ "$ELAPSED" -lt "$SMOKE_TIMEOUT" ]; do
    sleep 2
    ELAPSED=$((ELAPSED + 2))

    kill -0 "$QEMU_PID" 2>/dev/null || { QEMU_PID=""; fail "qemu exited prematurely"; }
    [ -f "$SMOKE_LOG" ] || continue

    if grep -q "PANIC:" "$SMOKE_LOG"; then
        fail "kernel panic"
    fi

    DONE=1
    for m in "${MARKERS[@]}"; do
        grep -q "$m" "$SMOKE_LOG" || { DONE=0; break; }
    done
    if [ "$DONE" = "1" ]; then
        echo "smoke-arm64: OK (${ELAPSED}s)"
        exit 0
    fi
done

fail "timeout after ${SMOKE_TIMEOUT}s waiting for: ${MARKERS[*]}"
