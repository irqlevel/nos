#!/bin/bash
# Panic reset test: a kernel booted with panic=N resets the machine N seconds
# after a panic's report, and one booted without it stays stopped with the
# report on its screen. The panic is the W^X probe's (wxprobe=text writes to
# .text: a page fault, early in boot), and QEMU runs with -no-reboot, so a
# reset is QEMU exiting by itself. A server whose only console is the
# network has nothing else to bring it back from a panic: the system image
# boots nos with panic=10, after which GRUB boots the server's own system.
# Usage: scripts/panic-test.sh [--skip-build] [--arch x86_64|aarch64]
# Env:   PANIC_TIMEOUT  seconds to wait for each boot to panic (default 120)
#        PANIC_HVF      set to 1 to run the arm64 boots under hvf (Apple Silicon)
# Exit codes: 0 = it reset when asked and only then, 1 = anything else.
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

PANIC_TIMEOUT="${PANIC_TIMEOUT:-120}"
# How long after the report the machine must have reset with panic=1, and how
# long it must still be there without: the wait, a reset, and room to spare.
RESET_WITHIN=20
STAYS_FOR=8
TMPDIR_PT="$(mktemp -d)"
QEMU_PID=""

cleanup() {
    [ -n "$QEMU_PID" ] && kill "$QEMU_PID" 2>/dev/null
    rm -rf "$TMPDIR_PT"
}
trap cleanup EXIT

fail() {
    echo "PANIC-TEST FAIL: $1"
    [ -n "${LOG:-}" ] && [ -f "$LOG" ] && {
        echo "--- last 20 lines of serial log ---"
        tail -n 20 "$LOG"
    }
    exit 1
}

if [ "$SKIP_BUILD" = "0" ]; then
    echo "panic-test: building $ARCH (docker, incremental)..."
    docker run --platform linux/amd64 --rm -v "$PWD:/src" -w /src nos-builder \
        bash -c "make nocheck ARCH=$ARCH" > "$TMPDIR_PT/build.log" 2>&1 \
        || { tail -n 30 "$TMPDIR_PT/build.log"; fail "build failed"; }
fi

# x86 takes its command line from the ISO's grub.cfg, so each boot needs an
# ISO of its own; arm64 takes -append and needs none.
if [ "$ARCH" = "x86_64" ]; then
    [ -f bin/kernel64.elf ] || fail "bin/kernel64.elf not found"
    echo "panic-test: building the two ISOs (docker)..."
    docker run --platform linux/amd64 --rm -v "$PWD:/src" -w /src nos-builder bash -c '
        set -e
        for name in reset stays; do
            args="wxprobe=text"
            [ "$name" = reset ] && args="$args panic=1"
            root="out/x86_64/panic-iso"
            rm -rf "$root"
            mkdir -p "$root/boot/grub"
            cp bin/kernel64.elf "$root/boot/kernel64.elf"
            printf "insmod all_video\nset timeout=0\nset default=0\nmenuentry \"nos\" {\n\tmultiboot2 /boot/kernel64.elf %s\n}\n" "$args" \
                > "$root/boot/grub/grub.cfg"
            grub-mkrescue -o "out/x86_64/panic-$name.iso" "$root"
            rm -rf "$root"
        done' > "$TMPDIR_PT/iso.log" 2>&1 \
        || { tail -n 20 "$TMPDIR_PT/iso.log"; fail "cannot build the ISOs"; }
elif [ "$ARCH" = "aarch64" ]; then
    [ -f nos-arm64.img ] || fail "nos-arm64.img not found"
    ACCEL_OPTS="-accel tcg -cpu cortex-a72"
    [ "${PANIC_HVF:-0}" = "1" ] && ACCEL_OPTS="-accel hvf -cpu host"
else
    fail "unknown arch: $ARCH"
fi

# One boot that is to reset itself, one that is to stay.
for NAME in reset stays; do
    LOG="$TMPDIR_PT/panic-$NAME.log"
    ARGS="wxprobe=text"
    [ "$NAME" = reset ] && ARGS="$ARGS panic=1"

    if [ "$ARCH" = "x86_64" ]; then
        qemu-system-x86_64 \
            -display none \
            -no-reboot \
            -m 1G \
            -smp 2 \
            -cdrom "out/x86_64/panic-$NAME.iso" \
            -serial "file:$LOG" \
            &
    else
        qemu-system-aarch64 \
            -M virt,gic-version=3 \
            -smp 2 \
            -m 1024 \
            -no-reboot \
            $ACCEL_OPTS \
            -kernel nos-arm64.img \
            -append "$ARGS" \
            -serial "file:$LOG" \
            -display none \
            &
    fi
    QEMU_PID=$!
    echo "panic-test: $ARCH $ARGS (log: $LOG)"

    ELAPSED=0
    PANICKED=0
    while [ "$ELAPSED" -lt "$PANIC_TIMEOUT" ]; do
        sleep 1
        ELAPSED=$((ELAPSED + 1))
        [ -f "$LOG" ] || continue
        if grep -q "^PANIC:" "$LOG"; then
            PANICKED=1
            break
        fi
        kill -0 "$QEMU_PID" 2>/dev/null || fail "$NAME: QEMU went away before the kernel panicked"
    done
    [ "$PANICKED" = "1" ] || fail "$NAME: no panic within ${PANIC_TIMEOUT}s"

    if [ "$NAME" = reset ]; then
        WAITED=0
        while kill -0 "$QEMU_PID" 2>/dev/null && [ "$WAITED" -lt "$RESET_WITHIN" ]; do
            sleep 1
            WAITED=$((WAITED + 1))
        done
        kill -0 "$QEMU_PID" 2>/dev/null && fail "reset: still running ${RESET_WITHIN}s after the panic, with panic=1"
        grep -q "resetting the machine in 1 s" "$LOG" || fail "reset: it reset without saying so"
        echo "panic-test: reset ${WAITED}s after the panic"
    else
        sleep "$STAYS_FOR"
        kill -0 "$QEMU_PID" 2>/dev/null || fail "stays: reset by itself without panic="
        grep -q "resetting the machine" "$LOG" && fail "stays: said it would reset, without panic="
        echo "panic-test: still stopped ${STAYS_FOR}s after the panic, as asked"
    fi
    kill "$QEMU_PID" 2>/dev/null
    wait "$QEMU_PID" 2>/dev/null
    QEMU_PID=""
done

echo "panic-test: OK ($ARCH)"
exit 0
