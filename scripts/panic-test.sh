#!/bin/bash
# Panic reset test: a kernel booted with panic=N resets the machine N seconds
# after a panic's report, and one booted without it stays stopped with the
# report on its screen. The panic is the W^X probe's (wxprobe=text writes to
# .text: a page fault, early in boot), and QEMU runs with -no-reboot, so a
# reset is QEMU exiting by itself. A server whose only console is the
# network has nothing else to bring it back from a panic: the system image
# boots nos with panic=10, after which GRUB boots the server's own system.
#
# And a third boot, the panic a running machine has from a stack that is no
# task's, with preemption on: x86's double fault, a real one (`panic df`: a
# stack pointer nothing can use, so the report runs on the IST stack and the
# stack it came from cannot be read), and on arm64, whose exceptions have no
# stack of their own, a panic run on a static one (`panic offstack`), each
# typed at the UDP shell. With the netconsole on, its report has to reach a
# collector -- on the Hetzner boxes the only console there is -- and panic=1
# has to reset the machine. Until the lock path stopped asking which task
# held a lock once a panic had begun, such a report stopped at its first
# lock, a panic inside the panic: on the network nothing, and no reset; and
# a double fault's stopped again at the walk of the stack it came from,
# until a fault in that walk came back to the report.
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
# The third boot's UDP shell, and the collector its netconsole sends to
SHELL_PORT=9103
NC_PORT=6670
OFFSTACK_ARGS="panic=1 dhcp=auto udpshell=$SHELL_PORT netconsole=10.0.2.2:$NC_PORT"
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
    echo "panic-test: building the three ISOs (docker)..."
    docker run --platform linux/amd64 --rm -v "$PWD:/src" -w /src -e OFFSTACK_ARGS="$OFFSTACK_ARGS" nos-builder bash -c '
        set -e
        for name in reset stays offstack; do
            case "$name" in
                reset) args="wxprobe=text panic=1" ;;
                stays) args="wxprobe=text" ;;
                offstack) args="$OFFSTACK_ARGS" ;;
            esac
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

# The third boot, driven from Python: QEMU its child, the netconsole's
# collector, the command typed once the collector has heard from the machine
# (its address resolved, which the report's flush needs), and what came out.
ACCEL="${ACCEL_OPTS:-}"
python3 - "$ARCH" "$TMPDIR_PT/panic-offstack.log" "out/x86_64/panic-offstack.iso" \
    "$SHELL_PORT" "$NC_PORT" "$PANIC_TIMEOUT" "$RESET_WITHIN" "$OFFSTACK_ARGS" "$ACCEL" <<'PY' || exit 1
import os, re, socket, struct, subprocess, sys, threading, time

arch, log, iso = sys.argv[1], sys.argv[2], sys.argv[3]
shell_port, nc_port, timeout, reset_within = (int(a) for a in sys.argv[4:8])
args, accel = sys.argv[8], sys.argv[9].split()
SHELL_MAGIC = 0x4E4F5348
SHELL_HDR = struct.Struct("!IIHHHH")   # scripts/udpsh.py
NC_HDR = 8                             # "NOSC" and the datagram's number

got = []
done = threading.Event()
sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
sock.bind(("0.0.0.0", nc_port))
sock.settimeout(0.5)


def collect():
    while not done.is_set():
        try:
            data, _ = sock.recvfrom(65536)
        except socket.timeout:
            continue
        got.append(data[NC_HDR:].decode(errors="replace"))


threading.Thread(target=collect, daemon=True).start()

if arch == "x86_64":
    argv = ["qemu-system-x86_64", "-display", "none", "-no-reboot", "-m", "1G", "-smp", "2",
            "-cdrom", iso, "-serial", "file:" + log,
            "-device", "virtio-net-pci,netdev=n0,disable-legacy=on,disable-modern=off"]
    command, said = "panic df", "EXC: DoubleFault"
else:
    argv = ["qemu-system-aarch64", "-M", "virt,gic-version=3", "-smp", "2", "-m", "1024",
            "-no-reboot"] + accel + ["-kernel", "nos-arm64.img", "-append", args,
            "-global", "virtio-mmio.force-legacy=false", "-device", "virtio-net-device,netdev=n0",
            "-serial", "file:" + log, "-display", "none"]
    command, said = "panic offstack", "off a task's stack"
argv += ["-netdev", "user,id=n0,hostfwd=udp::%d-:%d" % (shell_port, shell_port)]
p = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def serial():
    return open(log, errors="replace").read() if os.path.exists(log) else ""


def until(cond, secs):
    end = time.time() + secs
    while time.time() < end and not cond() and p.poll() is None:
        time.sleep(0.5)
    return cond()


def fail(why):
    print("PANIC-TEST FAIL: offstack: " + why)
    print("--- last 20 lines of serial log ---")
    print("".join(serial().splitlines(True)[-20:]), end="")
    sys.exit(1)


try:
    print("panic-test: %s %s, then %s (log: %s)" % (arch, args, command, log))
    if not until(lambda: "boot: complete" in serial(), timeout):
        fail("no boot within %ds" % timeout)
    if not until(lambda: got, 60):
        fail("the collector heard nothing from the netconsole")
    body = command.encode()
    shell = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    for seq in (1, 2, 3):
        shell.sendto(SHELL_HDR.pack(SHELL_MAGIC, seq, 0, 0, len(body), 0) + body,
                     ("127.0.0.1", shell_port))
        if until(lambda: "PANIC:" in serial(), 10):
            break
    else:
        fail("no panic after %r" % command)
    start = time.time()
    if not until(lambda: p.poll() is not None, reset_within):
        fail("still running %ds after the panic, with panic=1" % reset_within)
    print("panic-test: reset %ds after the panic" % (time.time() - start))
    time.sleep(1)
    text, sent = serial(), "".join(got)
    if said not in text:
        fail("the panic is not the one asked for: no %r" % said)
    if "resetting the machine in 1 s" not in text:
        fail("it reset without saying so")
    if said not in sent or "Backtrace" not in sent:
        fail("its report never reached the netconsole's collector")
    if arch == "x86_64":
        if "the stack it came from cannot be read" not in text:
            fail("the walk of the stack it came from did not say it could not be read")
        if not re.search(r"Cpu \d+ backtrace:", text) or not re.search(r"Cpu \d+ backtrace:", sent):
            fail("the other CPU's stack is not in the report")
    print("panic-test: its report reached the collector, %d bytes of it" % len(sent))
finally:
    done.set()
    if p.poll() is None:
        p.kill()
        p.wait()
PY

echo "panic-test: OK ($ARCH)"
exit 0
