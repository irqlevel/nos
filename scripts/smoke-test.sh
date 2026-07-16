#!/bin/bash
# Boot smoke test for the x86_64 kernel: build (in Docker), boot headless
# under QEMU TCG with virtio-blk(modern)/virtio-scsi(legacy)/nvme/net/rng
# attached, and assert the serial log reaches the boot-success markers.
#
# Usage: scripts/smoke-test.sh [--skip-build]
# Env:   SMOKE_TIMEOUT  seconds to wait for boot markers (default 300)
#        SMOKE_QEMU     qemu binary (default qemu-system-x86_64)
#        SMOKE_LOG      serial log path (default <tmpdir>/nos.log)
#
# Exit codes: 0 = all markers seen, 1 = panic/timeout/build failure.
set -u
cd "$(dirname "$0")/.."

SKIP_BUILD=0
[ "${1:-}" = "--skip-build" ] && SKIP_BUILD=1

SMOKE_TIMEOUT="${SMOKE_TIMEOUT:-300}"
SMOKE_QEMU="${SMOKE_QEMU:-qemu-system-x86_64}"

TMPDIR_SMOKE="$(mktemp -d)"
SMOKE_LOG="${SMOKE_LOG:-$TMPDIR_SMOKE/nos.log}"
QEMU_PID=""

cleanup() {
    [ -n "$QEMU_PID" ] && kill "$QEMU_PID" 2>/dev/null
    rm -rf "$TMPDIR_SMOKE"
}
trap cleanup EXIT

fail() {
    echo "SMOKE FAIL: $1"
    if [ -f "$SMOKE_LOG" ]; then
        echo "--- last 30 lines of serial log ---"
        tail -n 30 "$SMOKE_LOG"
    fi
    exit 1
}

if [ "$SKIP_BUILD" = "0" ]; then
    echo "smoke: building (docker, incremental)..."
    docker run --platform linux/amd64 --rm -v "$PWD:/src" -w /src nos-builder \
        bash -c 'make rust && make nocheck' > "$TMPDIR_SMOKE/build.log" 2>&1 \
        || { tail -n 30 "$TMPDIR_SMOKE/build.log"; fail "build failed"; }
fi

[ -f nos.iso ] || fail "nos.iso not found"

# Scratch disks: exercise virtio-blk (modern), virtio-scsi (legacy) and the
# Rust NVMe/MSI-X path on every smoke boot.
qemu-img create -q -f qcow2 "$TMPDIR_SMOKE/blk0.qcow2" 256M
qemu-img create -q -f qcow2 "$TMPDIR_SMOKE/scsi0.qcow2" 256M
qemu-img create -q -f qcow2 "$TMPDIR_SMOKE/nvme0.qcow2" 256M

echo "smoke: booting (log: $SMOKE_LOG, timeout: ${SMOKE_TIMEOUT}s)..."
"$SMOKE_QEMU" \
    -display none \
    -m 1G \
    -smp 4 \
    -cdrom nos.iso \
    -serial "file:$SMOKE_LOG" \
    -drive "file=$TMPDIR_SMOKE/blk0.qcow2,format=qcow2,id=drive0,if=none" \
    -device virtio-blk-pci,drive=drive0,disable-legacy=on,disable-modern=off \
    -drive "file=$TMPDIR_SMOKE/scsi0.qcow2,format=qcow2,id=scsi0,if=none" \
    -device virtio-scsi-pci,id=scsi,disable-legacy=off,disable-modern=on \
    -device scsi-hd,drive=scsi0,bus=scsi.0,channel=0,scsi-id=0,lun=0 \
    -drive "file=$TMPDIR_SMOKE/nvme0.qcow2,format=qcow2,id=nvme0,if=none" \
    -device nvme,serial=smoke0,drive=nvme0 \
    -device virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off \
    -netdev user,id=net0 \
    -device virtio-rng-pci \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
    &
QEMU_PID=$!

# Success requires all of these, which the kernel prints in this order:
#   "After test"        - Test::Test() self-tests passed (early boot, BSP)
#   "Preempt is now on" - SMP bringup + scheduler alive
#   "boot: complete"    - shells started, kernel reached the idle loop
MARKERS=("After test" "Preempt is now on" "boot: complete")

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
        echo "smoke: OK (${ELAPSED}s)"
        exit 0
    fi
done

fail "timeout after ${SMOKE_TIMEOUT}s waiting for: ${MARKERS[*]}"
