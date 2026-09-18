#!/usr/bin/env python3
"""netconsole test: boot with the kernel log streaming to a UDP collector and
check what arrives.

On the two Hetzner machines this is the only console there is, so what is
checked here is what someone debugging a dead machine depends on: that the
boot log queued before the link came up arrives once it does, that the
sequence numbers are contiguous so a gap can be told from a machine that
stopped, that lines produced after link-up keep coming, and that a panic's
report gets out -- which is the one time it matters most, and the one time
the drain task is not running to send it.

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


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tcg", action="store_true")
    ap.add_argument("--keep", action="store_true", help="leave the log behind")
    args = ap.parse_args()

    tmp = tempfile.mkdtemp(prefix="nos-netcon-")
    image = os.path.join(tmp, "root.img")
    log = os.path.join(tmp, "serial.log")
    pt.mkrootfs(image)

    collector = Collector(COLLECT_PORT)

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

    p = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
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
        if p.poll() is None:
            p.send_signal(signal.SIGTERM)
            try:
                p.wait(10)
            except subprocess.TimeoutExpired:
                p.kill()

    if args.keep:
        out = os.path.join(tmp, "netconsole.log")
        open(out, "w").write(collector.text())
        print("collected log at " + out)
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
