#!/usr/bin/env python3
"""netconsole test: boot with the kernel log streaming to a UDP collector and
check what arrives.

On the two Hetzner machines this is the only console there is, so what is
checked here is what someone debugging a dead machine depends on: that the
boot log queued before the link came up arrives once it does, that the
sequence numbers are contiguous so a gap can be told from a machine that
stopped, that lines produced after link-up keep coming, and that a panic's
report gets out -- which is the one time it matters most, and the one time
the drain task is not running to send it. Then a second boot is powered off,
and the halt's own lines must arrive: the netconsole stops after the modules
and the filesystems have gone -- a module's exit may wait minutes for
guests to turn themselves off, which on such a machine nothing else would
show -- and sends what it still holds as it stops, down to its own last
line.

    scripts/netconsole-test.py [--tcg] [--keep]

arm64 only, because it drives the shell over UDP to trigger the panic.
Exit code 0 = every check passed.
"""

import argparse
import importlib.util
import os
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

spec = importlib.util.spec_from_file_location("pt", os.path.join(HERE, "parttest.py"))
pt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pt)

SHELL_PORT = pt.SHELL_PORT
COLLECT_PORT = 6789

MAGIC = b"NOSC"
HDR_LEN = 8


class Collector:
    """What scripts/netconsole.py does, enough of it to judge the stream."""

    def __init__(self, port):
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.bind(("0.0.0.0", port))
        self.sock.settimeout(0.5)
        self.lines = []
        self.seqs = []
        self.headerless = 0
        self.running = True
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def run(self):
        partial = b""
        while self.running:
            try:
                data, _ = self.sock.recvfrom(4096)
            except socket.timeout:
                continue
            except OSError:
                break

            if data.startswith(MAGIC) and len(data) >= HDR_LEN:
                self.seqs.append(int.from_bytes(data[4:8], "little"))
                text = data[HDR_LEN:]
            else:
                self.headerless += 1
                text = data

            partial += text
            while b"\n" in partial:
                line, partial = partial.split(b"\n", 1)
                self.lines.append(line.decode("utf-8", "replace"))

    def stop(self):
        self.running = False
        try:
            self.sock.close()
        except OSError:
            pass
        self.thread.join(2)

    def text(self):
        return "\n".join(self.lines)

    def gaps(self):
        """Sequence numbers that never arrived, in the range that did."""
        if not self.seqs:
            return []
        seen = set(self.seqs)
        return [n for n in range(min(self.seqs), max(self.seqs) + 1) if n not in seen]


def wait_until(predicate, timeout):
    start = time.time()
    while time.time() - start < timeout:
        if predicate():
            return True
        time.sleep(0.2)
    return False


def boot(args, tmp, name):
    """nos with its log streaming to the collector, and its serial log."""
    image = os.path.join(tmp, name + ".img")
    log = os.path.join(tmp, name + ".log")
    pt.mkrootfs(image)

    import platform
    hvf = platform.system() == "Darwin" and platform.machine() == "arm64" and not args.tcg
    accel = ["-accel", "hvf", "-cpu", "host"] if hvf else ["-accel", "tcg", "-cpu", "cortex-a72"]
    argv = ["qemu-system-aarch64", "-M", "virt,gic-version=3", "-smp", "4", "-m", "1024"] + accel + [
        "-kernel", os.path.join(ROOT, "nos-arm64.img"),
        "-append", "root=auto dhcp=auto udpshell=%d netconsole=10.0.2.2:%d"
                   % (SHELL_PORT, COLLECT_PORT),
        "-global", "virtio-mmio.force-legacy=false",
        "-drive", "file=%s,format=raw,id=hd0,if=none" % image,
        "-device", "virtio-blk-device,drive=hd0",
        "-device", "virtio-net-device,netdev=n0",
        "-netdev", "user,id=n0,hostfwd=udp::%d-:%d" % (SHELL_PORT, SHELL_PORT),
        "-serial", "file:" + log, "-display", "none"]
    return subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL), log


def end(p):
    if p.poll() is None:
        p.send_signal(signal.SIGTERM)
        try:
            p.wait(10)
        except subprocess.TimeoutExpired:
            p.kill()


def halt(args, tmp):
    """A boot powered off from the shell over UDP: the halt's own lines --
    the unmount, after every module has gone, and the netconsole's last --
    must reach the collector, with no gap in the numbers."""
    collector = Collector(COLLECT_PORT)
    p, log = boot(args, tmp, "halt")
    try:
        if not pt.check("boots again, to be powered off", pt.wait_log(log, "boot: complete", 300)):
            return collector.text()
        pt.check("its log arrives", wait_until(lambda: len(collector.lines) > 50, 60), "%d lines" % len(collector.lines))
        sh = pt.Shell(timeout=5)
        try:
            sh.run("poweroff")
        except Exception:
            # The shell over UDP is the first thing the halt stops: its
            # answer may never come.
            pass
        off = wait_until(lambda: p.poll() is not None, 120)
        pt.check("the machine powers itself off", off and p.returncode == 0, "qemu %s" % p.poll())
        tail = lambda: "\n".join(collector.lines[-30:])
        pt.check("the halt's own lines reach the collector: the unmount, after the modules have gone",
                 wait_until(lambda: "vfs: unmounting /" in collector.text(), 10), tail())
        pt.check("down to the netconsole's own last line, sent as it stops",
                 wait_until(lambda: "netconsole: stopping -- the last line sent" in collector.text(), 10), tail())
        pt.check("with no gap in the numbers", collector.gaps() == [], str(collector.gaps()[:10]))
    finally:
        collector.stop()
        end(p)
    return collector.text()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tcg", action="store_true")
    ap.add_argument("--keep", action="store_true", help="leave the log behind")
    args = ap.parse_args()

    tmp = tempfile.mkdtemp(prefix="nos-netcon-")

    collector = Collector(COLLECT_PORT)
    p, log = boot(args, tmp, "root")
    try:
        if not pt.check("reaches the shell", pt.wait_log(log, "boot: complete", 300)):
            return 1

        # The backlog goes out once DHCP has given the device an address.
        pt.check("the boot log arrives once the link is up",
                 wait_until(lambda: len(collector.lines) > 50, 60),
                 "%d lines" % len(collector.lines))
        pt.check("including lines from before the network existed",
                 any("Page allocator" in l or "paging" in l.lower()
                     or "cpu" in l.lower() for l in collector.lines[:200]),
                 "\n".join(collector.lines[:5]))
        pt.check("every datagram carries the header the collector reads",
                 collector.headerless == 0, "%d without one" % collector.headerless)
        pt.check("the sequence numbers have no gaps",
                 collector.gaps() == [], str(collector.gaps()[:10]))

        # Something logged after link-up has to keep coming.
        before = len(collector.lines)
        sh = pt.Shell()
        sh.run("loglevel 1")
        sh.run("net")
        pt.check("lines logged after link-up arrive too",
                 wait_until(lambda: len(collector.lines) > before, 30),
                 "%d then %d" % (before, len(collector.lines)))

        # The panic report is the one that matters most, and the drain task
        # is not the one that sends it.
        marker_before = collector.text()
        sh.sock.settimeout(5)
        try:
            sh.run("panic pf")
        except Exception:
            pass

        pt.check("a panic gets its report out",
                 wait_until(lambda: "PANIC" in collector.text(), 60),
                 collector.text()[len(marker_before):][-400:])
        pt.check("with the backtrace after it",
                 wait_until(lambda: "Backtrace" in collector.text(), 30),
                 collector.text()[-400:])
    finally:
        collector.stop()
        end(p)

    halted = halt(args, tmp)

    if args.keep:
        out = os.path.join(tmp, "netconsole.log")
        open(out, "w").write(collector.text())
        open(os.path.join(tmp, "netconsole-halt.log"), "w").write(halted)
        print("collected logs at " + out + " and netconsole-halt.log beside it")
    else:
        subprocess.run(["rm", "-rf", tmp])

    print()
    if pt.failures:
        print("FAILED: " + ", ".join(pt.failures))
        return 1
    print("netconsole-test: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
