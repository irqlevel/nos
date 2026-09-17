#!/bin/bash
# W^X boot test: boot the kernel once per probe and assert the machine dies
# on the fault the probe asks for, at the address the probe names.
#
#   wxprobe=text  writes to .text          -> page fault / data abort
#   wxprobe=heap  calls into a heap page   -> page fault / instruction abort
#
# A kernel that keeps running prints "SUCCEEDED (W^X broken!)" instead, which
# is what this test is here to catch: the heap probe is the one that fails if
# MapRangeLocked ever stops setting NX on the leaves it writes.
#
# Usage: scripts/wx-test.sh [--skip-build] [--arch x86_64|aarch64]
# Env:   WX_TIMEOUT  seconds to wait for each boot to fault (default 120)
#        WX_HVF      set to 1 to run the arm64 boots under hvf (Apple Silicon)
#
# Exit codes: 0 = every probe faulted as it should, 1 = anything else.
set -u
cd "$(dirname "$0")/.."

SKIP_BUILD=0
ARCH=x86_64
while [ $# -gt 0 ]; do
    case "$1" in
        --skip-build) SKIP_BUILD=1 ;;
        --arch) shift; ARCH="${1:-}" ;;
        *) echo "unknown argument: $1"; exit 1 ;;
    esac
    shift
done

WX_TIMEOUT="${WX_TIMEOUT:-120}"
TMPDIR_WX="$(mktemp -d)"
QEMU_PID=""

cleanup() {
    [ -n "$QEMU_PID" ] && kill "$QEMU_PID" 2>/dev/null
    rm -f out/x86_64/wxprobe-text.iso out/x86_64/wxprobe-heap.iso
    rm -rf "$TMPDIR_WX"
}
trap cleanup EXIT

fail() {
    echo "WX FAIL: $1"
    [ -n "${LOG:-}" ] && [ -f "$LOG" ] && {
        echo "--- last 20 lines of serial log ---"
        tail -n 20 "$LOG"
    }
    exit 1
}

if [ "$SKIP_BUILD" = "0" ]; then
    echo "wx-test: building $ARCH (docker, incremental)..."
    docker run --platform linux/amd64 --rm -v "$PWD:/src" -w /src nos-builder \
        bash -c "make nocheck ARCH=$ARCH" > "$TMPDIR_WX/build.log" 2>&1 \
        || { tail -n 30 "$TMPDIR_WX/build.log"; fail "build failed"; }
fi

# x86 takes its command line from the ISO's grub.cfg, so each probe needs an
# ISO of its own; arm64 takes -append and needs none.
if [ "$ARCH" = "x86_64" ]; then
    [ -f bin/kernel64.elf ] || fail "bin/kernel64.elf not found"
    echo "wx-test: building the probe ISOs (docker)..."
    docker run --platform linux/amd64 --rm -v "$PWD:/src" -w /src nos-builder bash -c '
        set -e
        for probe in text heap; do
            root="out/x86_64/wxiso"
            rm -rf "$root"
            mkdir -p "$root/boot/grub"
            cp bin/kernel64.elf "$root/boot/kernel64.elf"
            printf "insmod all_video\nset timeout=0\nset default=0\nmenuentry \"nos\" {\n\tmultiboot2 /boot/kernel64.elf wxprobe=%s\n}\n" "$probe" \
                > "$root/boot/grub/grub.cfg"
            grub-mkrescue -o "out/x86_64/wxprobe-$probe.iso" "$root"
            rm -rf "$root"
        done' > "$TMPDIR_WX/iso.log" 2>&1 \
        || { tail -n 20 "$TMPDIR_WX/iso.log"; fail "cannot build the probe ISOs"; }
elif [ "$ARCH" = "aarch64" ]; then
    [ -f nos-arm64.img ] || fail "nos-arm64.img not found"
    ACCEL_OPTS="-accel tcg -cpu cortex-a72"
    [ "${WX_HVF:-0}" = "1" ] && ACCEL_OPTS="-accel hvf -cpu host"
else
    fail "unknown arch: $ARCH"
fi

# One boot per probe: it has to fault, and at the address it printed.
for PROBE in text heap; do
    LOG="$TMPDIR_WX/wx-$PROBE.log"

    if [ "$ARCH" = "x86_64" ]; then
        qemu-system-x86_64 \
            -display none \
            -m 1G \
            -smp 2 \
            -cdrom "out/x86_64/wxprobe-$PROBE.iso" \
            -serial "file:$LOG" \
            &
    else
        qemu-system-aarch64 \
            -M virt,gic-version=3 \
            -smp 2 \
            -m 1024 \
            $ACCEL_OPTS \
            -kernel nos-arm64.img \
            -append "wxprobe=$PROBE" \
            -serial "file:$LOG" \
            -display none \
            &
    fi
    QEMU_PID=$!
    echo "wx-test: $ARCH wxprobe=$PROBE (log: $LOG)"

    ELAPSED=0
    DIED=0
    while [ "$ELAPSED" -lt "$WX_TIMEOUT" ]; do
        sleep 1
        ELAPSED=$((ELAPSED + 1))
        [ -f "$LOG" ] || continue
        grep -q "SUCCEEDED" "$LOG" && fail "$PROBE probe: the kernel kept running -- W^X is broken"
        if grep -q "^PANIC:" "$LOG"; then
            DIED=1
            break
        fi
    done

    kill "$QEMU_PID" 2>/dev/null
    QEMU_PID=""

    [ "$DIED" = "1" ] || fail "$PROBE probe: no fault within ${WX_TIMEOUT}s"

    grep -q "W^X probe: " "$LOG" || fail "$PROBE probe: never ran (no probe line)"

    # The fault has to be the probe's, not some other one: the panic names
    # the address the probe printed (x86 cr2, arm64 far).
    ADDR="$(grep "W^X probe: " "$LOG" | tail -n 1 | grep -o "0x[0-9A-Fa-f]*" | head -n 1)"
    [ -n "$ADDR" ] || fail "$PROBE probe: no address in the probe line"
    grep "^PANIC:" "$LOG" | grep -qi "$ADDR" \
        || { echo "--- panic ---"; grep "^PANIC:" "$LOG"; fail "$PROBE probe: the panic is not about $ADDR"; }

    echo "wx-test: $PROBE probe faulted at $ADDR (${ELAPSED}s)"
done

echo "wx-test: OK ($ARCH, both probes)"
exit 0
