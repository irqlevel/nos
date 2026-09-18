#!/usr/bin/env python3
"""netload test: hammer the kernel's UDP load target from here and check
every datagram that comes back -- and that what did not come back was never
owed.

`netload` answers from inside the receive path: the reply is the frame that
arrived, kept past the callback, its addresses swapped where they lie, and
the replies of a batch go to the NIC together when the batch ends. None of
that is touched by any other test -- a smoke boot never starts it, and the
TCP and netconsole tests send through quite different code -- while all of
it is the kind of thing that fails quietly: a frame kept and never released
is a pool that runs dry an hour into a load test, and a batch that is never
handed over is an echo server that answers nothing and reports no error.

What is checked:

  - every echo is byte for byte a datagram that was sent, and none twice
  - a burst longer than the reply batch still comes back (the batch fills
    and is handed over mid-way rather than at the end)
  - the counters agree with what was sent, and nothing failed to transmit
  - sink mode answers nothing and counts everything
  - stopping gives the port back: more start/stop rounds than the listener
    table has slots
  - the frame pool ends where it began: no frame is left kept

    scripts/netload-test.py [--tcg] [--keep]

arm64 only, because it drives the shell over UDP. Exit code 0 = every check
passed.
"""

import argparse
import importlib.util
import os
import re
import select
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

spec = importlib.util.spec_from_file_location("pt", os.path.join(HERE, "parttest.py"))
pt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pt)

SHELL_PORT = pt.SHELL_PORT
LOAD_PORT = 9999

# One at a time, each waited for: every size a reply has to get right.
SIZES = [1, 2, 17, 64, 255, 256, 511, 512, 1000, 1399, 1400]
ROUND_TRIPS = 8

# More than the 64 replies the target gathers before it must hand them over.
BURST = 300
# UDP through the emulator's user network may lose some of a burst; an echo
# path that loses most of one is broken.
BURST_MIN = BURST * 8 // 10

# More rounds than a device has listener slots (16).
ROUNDS = 20


def payload(seq, size):
    """A datagram that says which one it is, and whose every byte can be
    told from its neighbours'."""
    head = struct.pack(">I", seq)
    body = bytes((seq * 31 + i * 7) & 0xFF for i in range(max(size - len(head), 0)))
    return (head + body)[:max(size, 1)] if size < len(head) else head + body


def collect(sock, seconds):
    got = []
    deadline = time.time() + seconds
    while True:
        left = deadline - time.time()
        if left <= 0:
            return got
        ready, _, _ = select.select([sock], [], [], left)
        if not ready:
            return got
        try:
            while True:
                got.append(sock.recv(4096))
        except BlockingIOError:
            pass


def stats(sh):
    out = sh.run("netload")
    rx = re.search(r"rx (\d+) packets", out)
    tx = re.search(r"tx (\d+) packets, (\d+) failed", out)
    if not rx or not tx:
        return None, out
    return (int(rx.group(1)), int(tx.group(1)), int(tx.group(2))), out


def in_flight(sh):
    out = sh.run("netpool")
    found = re.search(r"in flight (\d+)", out)
    return (int(found.group(1)) if found else None), out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tcg", action="store_true")
    ap.add_argument("--keep", action="store_true")
    ap.add_argument("--nic", choices=["virtio", "igb"], default="virtio",
                    help="the network card: igb is QEMU's 82576, which the Rust igb driver claims")
    args = ap.parse_args()

    tmp = tempfile.mkdtemp(prefix="nos-netload-")
    image = os.path.join(tmp, "root.img")
    log = os.path.join(tmp, "serial.log")
    pt.mkrootfs(image)

    import platform
    hvf = platform.system() == "Darwin" and platform.machine() == "arm64" and not args.tcg
    accel = ["-accel", "hvf", "-cpu", "host"] if hvf else ["-accel", "tcg", "-cpu", "cortex-a72"]
    argv = ["qemu-system-aarch64", "-M", "virt,gic-version=3", "-smp", "4", "-m", "1024"] + accel + [
        "-kernel", os.path.join(ROOT, "nos-arm64.img"),
        "-append", "root=auto dhcp=auto udpshell=%d" % SHELL_PORT,
        "-global", "virtio-mmio.force-legacy=false",
        "-drive", "file=%s,format=raw,id=hd0,if=none" % image,
        "-device", "virtio-blk-device,drive=hd0",
        "-device", "igb,netdev=n0" if args.nic == "igb" else "virtio-net-device,netdev=n0",
        "-netdev", "user,id=n0,hostfwd=udp::%d-:%d,hostfwd=udp::%d-:%d" % (
            SHELL_PORT, SHELL_PORT, LOAD_PORT, LOAD_PORT),
        "-serial", "file:" + log, "-display", "none"]

    p = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        if not pt.check("reaches the shell", pt.wait_log(log, "boot: complete", 300)):
            return 1
        time.sleep(12)   # let the boot-time DHCP finish
        sh = pt.Shell()
        sh.sock.settimeout(60)

        held_before, out = in_flight(sh)
        pt.check("the frame pool reports what is in flight", held_before is not None, out)

        out = sh.run("netload start %d" % LOAD_PORT)
        pt.check("netload starts in echo mode", "echo" in out and "listening" in out, out)

        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.setblocking(False)
        target = ("127.0.0.1", LOAD_PORT)

        # One at a time: a reply of every size, each byte of it checked.
        seq = 0
        sent_total = 0
        wrong = []
        for _ in range(ROUND_TRIPS):
            for size in SIZES:
                seq += 1
                data = payload(seq, size)
                sock.sendto(data, target)
                sent_total += 1
                back = collect(sock, 3)
                if back != [data]:
                    wrong.append("seq %d size %d: %d replies%s" % (
                        seq, size, len(back),
                        "" if not back else ", first differs" if back[0] != data else ""))
        pt.check("every datagram sent alone comes back as it went, once",
                 not wrong, "\n".join(wrong[:10]))

        # A burst: longer than the batch the target gathers.
        burst = {}
        for _ in range(BURST):
            seq += 1
            data = payload(seq, 64 + seq % 900)
            burst[data] = 0
            sock.sendto(data, target)
            sent_total += 1
        back = collect(sock, 5)
        strangers = [d for d in back if d not in burst]
        for d in back:
            if d in burst:
                burst[d] += 1
        twice = [d for d, n in burst.items() if n > 1]
        answered = sum(1 for n in burst.values() if n == 1)
        pt.check("a burst of %d comes back" % BURST, answered >= BURST_MIN,
                 "%d of %d" % (answered, BURST))
        pt.check("with nothing in it that was not sent", not strangers,
                 "%d unknown datagrams" % len(strangers))
        pt.check("and nothing twice", not twice, "%d duplicated" % len(twice))

        counts, out = stats(sh)
        pt.check("the target counted what arrived",
                 counts is not None and counts[0] >= len(SIZES) * ROUND_TRIPS + answered
                 and counts[0] <= sent_total, out)
        pt.check("answered all of it", counts is not None and counts[1] == counts[0], out)
        pt.check("and failed to transmit none", counts is not None and counts[2] == 0, out)

        out = sh.run("netload stop")
        pt.check("netload stops", "stopped" in out, out)
        seq += 1
        sock.sendto(payload(seq, 100), target)
        pt.check("and answers nothing once stopped", collect(sock, 2) == [])

        # Sink mode: counted, never answered.
        out = sh.run("netload start %d sink" % LOAD_PORT)
        pt.check("netload starts in sink mode", "sink" in out, out)
        for _ in range(50):
            seq += 1
            sock.sendto(payload(seq, 200), target)
        pt.check("a sink answers nothing", collect(sock, 3) == [])
        counts, out = stats(sh)
        pt.check("and counts everything",
                 counts is not None and 40 <= counts[0] <= 50 and counts[1] == 0, out)
        sh.run("netload stop")

        # The port comes back every time: more rounds than there are slots.
        started = 0
        for _ in range(ROUNDS):
            if "listening" in sh.run("netload start %d" % LOAD_PORT):
                started += 1
            seq += 1
            data = payload(seq, 300)
            sock.sendto(data, target)
            if collect(sock, 3) != [data]:
                started -= 1
            sh.run("netload stop")
        pt.check("%d rounds of start, echo, stop all work" % ROUNDS, started == ROUNDS,
                 "%d of %d" % (started, ROUNDS))

        # Every frame kept to answer in went out and came back to the pool.
        time.sleep(2)
        held_after, out = in_flight(sh)
        pt.check("no frame is left kept",
                 held_after is not None and held_before is not None
                 and held_after <= held_before + 2,
                 "%s in flight before, %s after\n%s" % (held_before, held_after, out))

        pt.check("the shell still answers after all of it", "eth0" in sh.run("net"))
    finally:
        if p.poll() is None:
            p.send_signal(signal.SIGTERM)
            try:
                p.wait(10)
            except subprocess.TimeoutExpired:
                p.kill()

    text = open(log, errors="replace").read()
    up = text.split("Stopping cpu")[0]
    pt.check("nothing panicked", "PANIC" not in up,
             "\n".join(l for l in up.splitlines() if "PANIC" in l))

    if args.keep:
        print("log at " + log)
    else:
        subprocess.run(["rm", "-rf", tmp])

    print()
    print("netload-test: " + ("OK" if not pt.failures else "FAILED: " + ", ".join(pt.failures)))
    return 0 if not pt.failures else 1


if __name__ == "__main__":
    sys.exit(main())
