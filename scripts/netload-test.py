#!/usr/bin/env python3
"""netload test: load the netload module, hammer its UDP target from here and
check every datagram that comes back -- and that what did not come back was
never owed; then turn it round, and check every datagram its source sends.

The target answers from inside the receive path: the reply is the frame that
arrived, kept past the callback, its addresses swapped where they lie, and
the replies of a batch go to the NIC together when the batch ends. None of
that is touched by any other test -- a smoke boot never starts it, and the
TCP and netconsole tests send through quite different code -- while all of
it is the kind of thing that fails quietly: a frame kept and never released
is a pool that runs dry an hour into a load test, and a batch that is never
handed over is an echo server that answers nothing and reports no error.

It is also the one module that lives on the receive path, so this is what
tests the typed listener a module is given -- a handler the listener owns,
frames lent, the end of a batch told -- and that unloading the module from
under a running target leaves neither a port taken nor a frame kept.

What is checked of the target:

  - every echo is byte for byte a datagram that was sent, and none twice
  - a burst longer than the reply batch still comes back (the batch fills
    and is handed over mid-way rather than at the end)
  - the counters agree with what was sent, and nothing failed to transmit
  - sink mode answers nothing and counts everything
  - stopping gives the port back: more start/stop rounds than the listener
    table has slots

of the source:

  - a paced run arrives whole: every datagram the size asked for, marked
    with its sender and a sequence number, no number twice, its filler
    intact, and the kernel's count of what it sent the same as what came
  - what is sent back to it is counted as echoes, and not answered
  - a run with no pace finishes, and accounts for every datagram it was
    asked for as either sent or failed
  - the shell goes on answering while a run with no pace floods: the source
    leaves room in the transmit queue for everybody else
  - an address nothing answers ARP for is refused, and nothing is sent
  - bad arguments are refused

and of the module:

  - rmmod with a target running takes it down: the port no longer answers
  - loaded again, it starts again on the same port
  - the frame pool ends where it began: no frame is left kept

and of the tick's receive poll, which nothing but `rxpoll=on` turns on:

  - booted without it, the tick has not polled once by the end of all that
  - booted again with it, the tick polls

    scripts/netload-test.py [--arch aarch64|x86_64] [--tcg] [--keep] [--nic igb]

All of that is arm64, because it drives the shell over UDP and the arm64
boot is the one whose command line carries one. `--arch x86_64` is the
short form: nos.iso, a root that carries the module and an /etc/rc that
loads it, starts the target and runs the source once -- no shell at all. It
checks what differs between the architectures rather than what does not:
that the module loads and binds, that its calls across the C ABI come back
right where the ABI is the other one -- two of them return a structure by
value -- and that the target echoes and the source's run arrives whole
there too.

Exit code 0 = every check passed.
"""

import argparse
import importlib.util
import os
import re
import select
import shutil
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

# Where the kernel's source sends, on this side of the emulator's network:
# the host is 10.0.2.2 to the guest, and what the guest sends there arrives
# on the loopback.
HOST_FROM_GUEST = "10.0.2.2"
SINK_PORT = 7777
# An address on the guest's subnet that the emulator does not answer ARP for.
NOBODY = "10.0.2.77"

# A paced run, slow enough for the emulator's user network to carry whole.
PACED_COUNT = 300
PACED_SIZE = 200
PACED_PPS = 500
PACED_TASKS = 2
# A run with no pace: more than the NIC's transmit queue holds, several times.
FLOOD_COUNT = 5000
# ... and one that goes on for a while, with the shell asked things meanwhile.
FLOOD_SECS = 4
FLOOD_ASKS = 10

SOURCE_MAGIC = b"NLD1"
SOURCE_HEADER = 16


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


def source_stats(sh):
    """What the source has counted: (sent, failed, echoes, done)."""
    out = sh.run("netload")
    sent = re.search(r"sent (\d+) packets, \d+ bytes, (\d+) failed", out)
    echoes = re.search(r"echoes (\d+) packets", out)
    if not sent or not echoes:
        return None, out
    return (int(sent.group(1)), int(sent.group(2)), int(echoes.group(1)), "done in" in out), out


def wait_done(sh, seconds):
    """The source's counts once it says it is done, or its last ones."""
    deadline = time.time() + seconds
    while True:
        counts, out = source_stats(sh)
        if (counts and counts[3]) or time.time() > deadline:
            return counts, out
        time.sleep(0.5)


def drain(sock, seconds, echo=False):
    """Every datagram that arrives within `seconds` of the last one, each
    sent back where it came from if `echo`."""
    got = []
    while True:
        ready, _, _ = select.select([sock], [], [], seconds)
        if not ready:
            return got
        try:
            while True:
                data, sender = sock.recvfrom(4096)
                got.append(data)
                if echo:
                    sock.sendto(data, sender)
        except BlockingIOError:
            pass


def misshapen(datagrams, size, tasks):
    """What is wrong with what the source sent, if anything: a list of
    complaints, and the (sender, sequence) pairs that were seen."""
    wrong, seen = [], {}
    for data in datagrams:
        if len(data) != size:
            wrong.append("%d bytes, not %d" % (len(data), size))
            continue
        if data[:4] != SOURCE_MAGIC:
            wrong.append("starts %r" % data[:4])
            continue
        sender, seq = struct.unpack(">IQ", data[4:SOURCE_HEADER])
        if sender >= tasks:
            wrong.append("sender %d of %d" % (sender, tasks))
        if data[SOURCE_HEADER:] != bytes(i & 0xFF for i in range(SOURCE_HEADER, size)):
            wrong.append("sender %d seq %d: filler damaged" % (sender, seq))
        seen[(sender, seq)] = seen.get((sender, seq), 0) + 1
    return wrong, seen


def in_flight(sh):
    out = sh.run("netpool")
    found = re.search(r"in flight (\d+)", out)
    return (int(found.group(1)) if found else None), out


def udp_socket():
    """A non-blocking socket with room for a whole burst of echoes. Under KVM
    the guest answers faster than the burst is sent, and Linux's default
    receive buffer -- about 208 KiB, charged per datagram with its overhead --
    holds some 220 of them: the rest were dropped by this host, and the test
    failed on its own socket. SO_RCVBUFFORCE (as root) passes rmem_max."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    for opt in (getattr(socket, "SO_RCVBUFFORCE", 33), socket.SO_RCVBUF):
        try:
            sock.setsockopt(socket.SOL_SOCKET, opt, 8 << 20)
            break
        except OSError:
            pass
    sock.setblocking(False)
    return sock


def host_drops():
    """UDP datagrams this host has dropped for a full socket buffer, when it
    says (Linux); None elsewhere."""
    try:
        with open("/proc/net/snmp") as f:
            rows = [line.split() for line in f if line.startswith("Udp:")]
        return int(rows[1][rows[0].index("RcvbufErrors")])
    except (OSError, ValueError, IndexError):
        return None


def burst_detail(answered, drops_before):
    after = host_drops()
    lost_here = "" if after is None or drops_before is None else \
        ", %d of them dropped by this host's socket buffer" % (after - drops_before)
    return "%d of %d%s" % (answered, BURST, lost_here)


def rx_polls(sh):
    """The receive passes the tick has asked for since boot, from `net`."""
    out = sh.run("net")
    found = re.search(r"rx polls (\d+)", out)
    return (int(found.group(1)) if found else None), out


def qemu_argv(args, image, log, extra=""):
    import platform
    hvf = platform.system() == "Darwin" and platform.machine() == "arm64" and not args.tcg
    accel = ["-accel", "hvf", "-cpu", "host"] if hvf else ["-accel", "tcg", "-cpu", "cortex-a72"]
    return ["qemu-system-aarch64", "-M", "virt,gic-version=3", "-smp", "4", "-m", "1024"] + accel + [
        "-kernel", os.path.join(ROOT, "nos-arm64.img"),
        "-append", ("root=auto dhcp=auto udpshell=%d %s" % (SHELL_PORT, extra)).strip(),
        "-global", "virtio-mmio.force-legacy=false",
        "-drive", "file=%s,format=raw,id=hd0,if=none" % image,
        "-device", "virtio-blk-device,drive=hd0",
        "-device", "igb,netdev=n0" if args.nic == "igb" else "virtio-net-device,netdev=n0",
        "-netdev", "user,id=n0,hostfwd=udp::%d-:%d,hostfwd=udp::%d-:%d" % (
            SHELL_PORT, SHELL_PORT, LOAD_PORT, LOAD_PORT),
        "-serial", "file:" + log, "-display", "none"]


def stop(p):
    if p.poll() is None:
        p.send_signal(signal.SIGTERM)
        try:
            p.wait(10)
        except subprocess.TimeoutExpired:
            p.kill()


def panicked(log):
    up = open(log, errors="replace").read().split("Stopping cpu")[0]
    return "\n".join(l for l in up.splitlines() if "PANIC" in l)


def x86(args):
    """nos.iso, with everything asked of the module from /etc/rc."""
    module = os.path.join(ROOT, "out", "x86_64", "modules", "netload.ko")
    if not os.path.exists(module):
        sys.exit("no %s -- make first" % module)

    tmp = tempfile.mkdtemp(prefix="nos-netload-")
    image = os.path.join(tmp, "root.img")
    log = os.path.join(tmp, "serial.log")
    rootdir = os.path.join(tmp, "rootdir")
    os.makedirs(os.path.join(rootdir, "etc"))
    shutil.copy(module, rootdir)
    with open(os.path.join(rootdir, "etc", "rc"), "w") as f:
        f.write("# netload-test.py\ninsmod /netload.ko\nnetload start %d\n"
                "netload send %s %d size=%d pps=%d count=%d tasks=%d\n" % (
                    LOAD_PORT, HOST_FROM_GUEST, SINK_PORT, PACED_SIZE, PACED_PPS, PACED_COUNT,
                    PACED_TASKS))
    subprocess.run([os.path.join(HERE, "mkrootfs.sh"), image, "64", rootdir, "1024"],
                   cwd=ROOT, check=True, stdout=subprocess.DEVNULL)

    # Bound before the boot: the source starts as soon as the network is up.
    sink = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sink.bind(("0.0.0.0", SINK_PORT))
    sink.setblocking(False)

    kvm = os.path.exists("/dev/kvm") and not args.tcg
    argv = ["qemu-system-x86_64", "-display", "none", "-m", "1G", "-smp", "4",
            "-cdrom", os.path.join(ROOT, "nos.iso"), "-serial", "file:" + log,
            "-drive", "file=%s,format=raw,id=drive0,if=none" % image,
            "-device", "virtio-blk-pci,drive=drive0,disable-legacy=on,disable-modern=off",
            "-device", ("igb,netdev=net0" if args.nic == "igb"
                        else "virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off"),
            "-netdev", "user,id=net0,hostfwd=udp::%d-:%d" % (LOAD_PORT, LOAD_PORT)]
    if kvm:
        argv += ["-enable-kvm", "-cpu", "host"]

    p = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        if not pt.check("boots", pt.wait_log(log, "boot: complete", 400)):
            return 1
        pt.check("the module loads and the target starts, from /etc/rc",
                 pt.wait_log(log, "netload: listening on udp %d, echo" % LOAD_PORT, 120))
        pt.check("and the source, its destination's address resolved",
                 pt.wait_log(log, "netload: sending to %s:%d" % (HOST_FROM_GUEST, SINK_PORT), 60),
                 "\n".join(l for l in open(log, errors="replace").read().splitlines() if "netload" in l))

        arrived = drain(sink, 5)
        wrong, seen = misshapen(arrived, PACED_SIZE, PACED_TASKS)
        pt.check("every datagram the source sent is the shape it should be", not wrong,
                 "\n".join(wrong[:10]))
        pt.check("none arrived twice", all(n == 1 for n in seen.values()))
        pt.check("and all of them arrived", len(seen) == PACED_COUNT,
                 "%d of %d" % (len(seen), PACED_COUNT))

        sock = udp_socket()
        target = ("127.0.0.1", LOAD_PORT)
        seq, wrong = 0, []
        for size in SIZES:
            seq += 1
            data = payload(seq, size)
            sock.sendto(data, target)
            back = collect(sock, 3)
            if back != [data]:
                wrong.append("seq %d size %d: %d replies" % (seq, size, len(back)))
        pt.check("every datagram sent alone comes back as it went, once", not wrong,
                 "\n".join(wrong))

        drops = host_drops()
        burst = {}
        for _ in range(BURST):
            seq += 1
            data = payload(seq, 64 + seq % 900)
            burst[data] = 0
            sock.sendto(data, target)
        back = collect(sock, 5)
        for d in back:
            if d in burst:
                burst[d] += 1
        answered = sum(1 for n in burst.values() if n == 1)
        pt.check("a burst of %d comes back" % BURST, answered >= BURST_MIN,
                 burst_detail(answered, drops))
        pt.check("with nothing in it that was not sent, and nothing twice",
                 all(d in burst for d in back) and all(n <= 1 for n in burst.values()))
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
    return 1 if pt.failures else 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--arch", choices=["aarch64", "x86_64"], default="aarch64")
    ap.add_argument("--tcg", action="store_true")
    ap.add_argument("--keep", action="store_true")
    ap.add_argument("--nic", choices=["virtio", "igb"], default="virtio",
                    help="the network card: igb is QEMU's 82576, which the Rust igb driver claims")
    args = ap.parse_args()
    if args.arch == "x86_64":
        return x86(args)

    module = os.path.join(ROOT, "out", "aarch64", "modules", "netload.ko")
    if not os.path.exists(module):
        sys.exit("no %s -- make ARCH=aarch64 first" % module)

    tmp = tempfile.mkdtemp(prefix="nos-netload-")
    image = os.path.join(tmp, "root.img")
    log = os.path.join(tmp, "serial.log")
    rootdir = os.path.join(tmp, "rootdir")
    os.makedirs(rootdir)
    shutil.copy(module, rootdir)
    subprocess.run([os.path.join(HERE, "mkrootfs.sh"), image, "64", rootdir, "1024"],
                   cwd=ROOT, check=True, stdout=subprocess.DEVNULL)

    p = subprocess.Popen(qemu_argv(args, image, log), cwd=ROOT,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        if not pt.check("reaches the shell", pt.wait_log(log, "boot: complete", 300)):
            return 1
        time.sleep(12)   # let the boot-time DHCP finish
        sh = pt.Shell()
        sh.sock.settimeout(60)

        held_before, out = in_flight(sh)
        pt.check("the frame pool reports what is in flight", held_before is not None, out)

        out = sh.run("netload")
        pt.check("there is no netload command until the module is loaded",
                 "not running" not in out and "listening" not in out, out)
        out = sh.run("insmod /netload.ko")
        if not pt.check("netload.ko loads", "loaded" in out, out):
            return 1
        pt.check("and says nothing is running", "not running" in sh.run("netload"))

        out = sh.run("netload start %d" % LOAD_PORT)
        pt.check("netload starts in echo mode", "echo" in out and "listening" in out, out)

        sock = udp_socket()
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
        drops = host_drops()
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
                 burst_detail(answered, drops))
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

        # ---- the source ----
        sink = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            sink.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4 << 20)
        except OSError:
            pass
        sink.bind(("0.0.0.0", SINK_PORT))
        sink.setblocking(False)

        for bad, why in (("netload send", "no address"),
                         ("netload send 10.0.2 %d" % SINK_PORT, "not a dotted quad"),
                         ("netload send %s 0" % HOST_FROM_GUEST, "port 0"),
                         ("netload send %s %d size=8" % (HOST_FROM_GUEST, SINK_PORT), "too small"),
                         ("netload send %s %d size=1473" % (HOST_FROM_GUEST, SINK_PORT), "too big"),
                         ("netload send %s %d tasks=0" % (HOST_FROM_GUEST, SINK_PORT), "no tasks"),
                         ("netload send %s %d tasks=4 count=3" % (HOST_FROM_GUEST, SINK_PORT),
                          "fewer datagrams than senders"),
                         ("netload send %s %d speed=9" % (HOST_FROM_GUEST, SINK_PORT),
                          "an option there is not")):
            out = sh.run(bad)
            pt.check("send is refused: " + why, "usage" in out and "sending to" not in out, out)
        pt.check("and a refused send leaves nothing running", "not running" in sh.run("netload"))

        out = sh.run("netload send %s %d" % (NOBODY, SINK_PORT))
        pt.check("an address nothing answers ARP for is refused",
                 "nothing answers" in out and "sending to" not in out, out)
        pt.check("and nothing was sent to it", "not running" in sh.run("netload"))

        # Paced: slow enough to arrive whole, so every datagram is owed.
        out = sh.run("netload send %s %d size=%d pps=%d count=%d tasks=%d" % (
            HOST_FROM_GUEST, SINK_PORT, PACED_SIZE, PACED_PPS, PACED_COUNT, PACED_TASKS))
        pt.check("netload sends", "sending to %s:%d" % (HOST_FROM_GUEST, SINK_PORT) in out, out)
        arrived = drain(sink, 3, echo=True)
        counts, out = wait_done(sh, 30)
        pt.check("a paced run finishes", counts is not None and counts[3], out)
        pt.check("having sent what it was asked to, and failed none of it",
                 counts is not None and counts[0] == PACED_COUNT and counts[1] == 0, out)

        wrong, seen = misshapen(arrived, PACED_SIZE, PACED_TASKS)
        pt.check("every datagram it sent is the shape it should be", not wrong, "\n".join(wrong[:10]))
        pt.check("none arrived twice", all(n == 1 for n in seen.values()),
                 "%d duplicated" % sum(1 for n in seen.values() if n > 1))
        pt.check("and all of them arrived", len(seen) == PACED_COUNT,
                 "%d of %d" % (len(seen), PACED_COUNT))
        per_sender = [sorted(seq for who, seq in seen if who == sender) for sender in range(PACED_TASKS)]
        pt.check("each sender's numbers run from 0 with no gap",
                 len(seen) != PACED_COUNT or all(seqs == list(range(len(seqs))) for seqs in per_sender),
                 [len(seqs) for seqs in per_sender])

        time.sleep(1)
        counts, out = source_stats(sh)
        pt.check("what was sent back to it is counted as echoes",
                 counts is not None and counts[2] >= len(arrived) * 9 // 10 and counts[2] <= len(arrived),
                 "%d sent back\n%s" % (len(arrived), out))
        pt.check("and none of them was answered", drain(sink, 2) == [])

        # The target and the source at once: one machine can be both.
        out = sh.run("netload start %d" % LOAD_PORT)
        pt.check("the target starts beside a finished source", "listening" in out, out)
        seq += 1
        data = payload(seq, 400)
        sock.sendto(data, target)
        pt.check("and echoes", collect(sock, 3) == [data])

        # No pace: faster than the wire takes, so some of it may fail -- but
        # every datagram asked for is accounted for, and the run ends.
        out = sh.run("netload send %s %d count=%d" % (HOST_FROM_GUEST, SINK_PORT, FLOOD_COUNT))
        pt.check("a new run takes a finished one's place", "sending to" in out, out)
        counts, out = wait_done(sh, 60)
        flood = drain(sink, 3)
        pt.check("a run with no pace finishes", counts is not None and counts[3], out)
        pt.check("and accounts for every datagram as sent or failed",
                 counts is not None and counts[0] + counts[1] == FLOOD_COUNT and counts[0] > 0, out)
        # One sender asks the queue for room before it builds a batch, so
        # next to nothing of what it builds should find the queue full.
        pt.check("losing next to none of them to a full transmit queue",
                 counts is not None and counts[1] <= FLOOD_COUNT // 100, out)
        wrong, seen = misshapen(flood, 64, 1)
        pt.check("what arrived of it is the shape it should be, none twice",
                 not wrong and all(n == 1 for n in seen.values()) and len(seen) <= FLOOD_COUNT,
                 "\n".join(wrong[:10]))
        print("  (%d of %d sent arrived through the emulator)" % (
            len(seen), counts[0] if counts else 0))

        # A source with no pace and no end, and the shell beside it. It
        # leaves room in the transmit queue for everybody else; one that did
        # not would keep the queue full, and every answer of the shell's
        # would be released for want of room -- on a machine whose only
        # console is the network, a load test that silences the console.
        out = sh.run("netload send %s %d secs=%d" % (HOST_FROM_GUEST, SINK_PORT, FLOOD_SECS))
        pt.check("a run with no pace and no count starts", "sending to" in out, out)
        sh.sock.settimeout(5)
        answered = 0
        for _ in range(FLOOD_ASKS):
            try:
                if "eth0" in sh.run("net"):
                    answered += 1
            except socket.timeout:
                pass
            drain(sink, 0.2)
        sh.sock.settimeout(60)
        pt.check("the shell answers every time while the source floods",
                 answered == FLOOD_ASKS, "%d of %d" % (answered, FLOOD_ASKS))
        counts, out = wait_done(sh, 60)
        drain(sink, 3)
        pt.check("and the run ends when its time is up", counts is not None and counts[3], out)
        pt.check("having lost next to nothing to a full queue",
                 counts is not None and counts[0] > FLOOD_COUNT
                 and counts[1] <= counts[0] // 100, out)

        seq += 1
        data = payload(seq, 300)
        sock.sendto(data, target)
        pt.check("the target still echoes after it", collect(sock, 3) == [data])

        # ---- the module's life ----
        out = sh.run("rmmod netload")
        pt.check("rmmod with a target running and a source finished", "unloaded" in out, out)
        seq += 1
        sock.sendto(payload(seq, 100), target)
        pt.check("takes the target down: the port no longer answers", collect(sock, 2) == [])
        pt.check("and the command with it", "not running" not in sh.run("netload"))

        out = sh.run("insmod /netload.ko")
        pt.check("it loads again", "loaded" in out, out)
        out = sh.run("netload start %d" % LOAD_PORT)
        pt.check("and starts on the port the last one had", "listening" in out, out)
        seq += 1
        data = payload(seq, 300)
        sock.sendto(data, target)
        pt.check("and echoes", collect(sock, 3) == [data])
        out = sh.run("rmmod netload")
        pt.check("and unloads", "unloaded" in out, out)
        pt.check("with no module left behind", "netload" not in sh.run("lsmod"))

        # Every frame kept to answer in went out and came back to the pool.
        time.sleep(2)
        held_after, out = in_flight(sh)
        pt.check("no frame is left kept",
                 held_after is not None and held_before is not None
                 and held_after <= held_before + 2,
                 "%s in flight before, %s after\n%s" % (held_before, held_after, out))

        pt.check("the shell still answers after all of it", "eth0" in sh.run("net"))

        # The tick looks at the receive path only when rxpoll=on asks. The
        # switch once lived in the C++ function the tick called; that went
        # with src/cpp/net, and from then on every tick polled. Nothing
        # failed: under a flood the receive softirq just moved back and forth
        # between the BSP and the queue vector's CPU, an IPI each time, and
        # took two CPUs where it had taken one.
        polls, out = rx_polls(sh)
        pt.check("booted without rxpoll=on, the tick never polled the receive path",
                 polls == 0, out)
    finally:
        stop(p)

    bad = panicked(log)
    pt.check("nothing panicked", not bad, bad)

    # And the switch still turns it on: a poll nothing can enable is as
    # silent a failure as one nothing can disable.
    poll_log = os.path.join(tmp, "serial-rxpoll.log")
    p = subprocess.Popen(qemu_argv(args, image, poll_log, "rxpoll=on"), cwd=ROOT,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        if pt.check("boots again, with rxpoll=on", pt.wait_log(poll_log, "boot: complete", 300)):
            time.sleep(12)   # the boot-time DHCP, as above; the tick goes on meanwhile
            sh = pt.Shell()
            sh.sock.settimeout(60)
            polls, out = rx_polls(sh)
            pt.check("and with it the tick polls the receive path",
                     polls is not None and polls > 0, out)
    finally:
        stop(p)

    bad = panicked(poll_log)
    pt.check("nothing panicked with the poll on", not bad, bad)

    if args.keep:
        print("logs at %s and %s" % (log, poll_log))
    else:
        subprocess.run(["rm", "-rf", tmp])

    print()
    print("netload-test: " + ("OK" if not pt.failures else "FAILED: " + ", ".join(pt.failures)))
    return 0 if not pt.failures else 1


if __name__ == "__main__":
    sys.exit(main())
