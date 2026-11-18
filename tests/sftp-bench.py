#!/usr/bin/env python3
"""What files are copied at over SFTP: a file into nos and back out with
OpenSSH's sftp, over QEMU's user network -- as it is, and with a round trip
added (QEMU's filter-buffer, which holds each way's frames and lets them go
every so often) -- on this tree's images, or another's: the tree before a
change, for the numbers against it.

    tests/sftp-bench.py                          arm64 under HVF: 64 MiB, round trips of 0 and ~40 ms
    tests/sftp-bench.py --mib 16 --runs 3 --hold-us 0 --hold-us 5000
    tests/sftp-bench.py --tree ../before         the images and sshd.ko of another built tree

Each run is a put and a get of the file, each a session of its own, timed
less what a session that copies nothing takes; the file must come back the
same. Says MB/s each way, every run's and the median, and the round trip the
kernel's `ping` of the gateway measures through the hold. The machine has
sshd's own window and its TCP's as built: a bench, not a gate. Needs a
built tree (`make ARCH=aarch64`), ssh, sftp, ssh-keygen and mkrootfs's mke2fs.
"""

import argparse
import functools
import hashlib
import http.server
import os
import platform
import re
import shutil
import signal
import socket
import statistics
import struct
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
sys.path.insert(0, HERE)

import parttest as pt  # noqa: E402  (tests/parttest.py)

SSH_PORT = 2252
HTTP_PORT = 8020
SHELL_PORT = 9020

# The UDP shell's framing (scripts/udpsh.py)
SHELL_MAGIC = 0x4E4F5348
SHELL_HDR = struct.Struct("!IIHHHH")
SHELL_LAST = 1


class Shell:
    """The UDP shell, answering whole."""

    def __init__(self, timeout=90):
        self.seq = 0
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.settimeout(timeout)

    def run(self, cmd):
        self.seq += 1
        body = cmd.encode()
        self.sock.sendto(SHELL_HDR.pack(SHELL_MAGIC, self.seq, 0, 0, len(body), 0) + body, ("127.0.0.1", SHELL_PORT))
        chunks, last = {}, None
        while True:
            data, _ = self.sock.recvfrom(4096)
            magic, seq, idx, flags, length, _ = SHELL_HDR.unpack(data[:SHELL_HDR.size])
            if magic != SHELL_MAGIC or seq != self.seq:
                continue
            chunks[idx] = data[SHELL_HDR.size:SHELL_HDR.size + length]
            if flags & SHELL_LAST:
                last = idx
            if last is not None and len(chunks) == last + 1:
                return b"".join(chunks[i] for i in range(last + 1)).decode(errors="replace")


def wait_log(log, marker, timeout):
    start = time.time()
    while time.time() - start < timeout:
        text = open(log, errors="replace").read() if os.path.exists(log) else ""
        if marker in text:
            return True
        if "PANIC" in text:
            sys.exit("sftp-bench: the kernel panicked:\n" + text[-3000:])
        time.sleep(0.5)
    return False


def serve_http(directory):
    handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=directory)
    handler.log_message = lambda *a: None
    httpd = http.server.ThreadingHTTPServer(("127.0.0.1", HTTP_PORT), handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    return httpd


class Bench:
    def __init__(self, args, tmp):
        self.args = args
        self.tmp = tmp
        self.key = os.path.join(tmp, "id_bench")
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "sftp-bench", "-f", self.key], check=True)
        self.big = os.path.join(tmp, "big.bin")
        with open(self.big, "wb") as f:
            for _ in range(args.mib):
                f.write(os.urandom(1 << 20))
        with open(self.big, "rb") as f:
            self.digest = hashlib.sha256(f.read()).hexdigest()

    def sftp(self, commands):
        batch = os.path.join(self.tmp, "batch")
        with open(batch, "w") as f:
            f.write("\n".join(commands) + "\n")
        start = time.monotonic()
        p = subprocess.run(["sftp", "-P", str(SSH_PORT), "-i", self.key, "-o", "IdentitiesOnly=yes",
                            "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=no",
                            "-o", "UserKnownHostsFile=/dev/null", "-o", "LogLevel=ERROR", "-b", batch,
                            "root@127.0.0.1"], capture_output=True, timeout=3600)
        took = time.monotonic() - start
        if p.returncode != 0:
            sys.exit("sftp-bench: sftp %s failed: %s" % ("; ".join(commands), p.stderr.decode(errors="replace")))
        return took

    def boot(self, hold_us):
        image = os.path.join(self.tmp, "root.img")
        empty = os.path.join(self.tmp, "empty")
        os.makedirs(empty, exist_ok=True)
        if os.path.exists(image):
            os.remove(image)
        size_mb = max(256, 3 * self.args.mib + 64)
        subprocess.run([os.path.join(ROOT, "scripts", "mkrootfs.sh"), image, str(size_mb), empty], check=True,
                       stdout=subprocess.DEVNULL)
        log = os.path.join(self.tmp, "serial.log")
        if os.path.exists(log):
            os.remove(log)
        hvf = platform.system() == "Darwin" and platform.machine() == "arm64" and not self.args.tcg
        accel = ["-accel", "hvf", "-cpu", "host"] if hvf else ["-accel", "tcg", "-cpu", "cortex-a72"]
        argv = ["qemu-system-aarch64", "-M", "virt,gic-version=3", "-smp", "4", "-m", "1024"] + accel + [
            "-kernel", os.path.join(self.args.tree, "nos-arm64.img"),
            "-append", "dhcp=auto dns=on root=auto udpshell=%d" % SHELL_PORT,
            "-global", "virtio-mmio.force-legacy=false",
            "-drive", "file=%s,format=raw,id=hd,if=none" % image, "-device", "virtio-blk-device,drive=hd",
            "-device", "virtio-net-device,netdev=net0",
            "-netdev", "user,id=net0,hostfwd=udp::%d-:%d,hostfwd=tcp::%d-:22" % (SHELL_PORT, SHELL_PORT, SSH_PORT),
            "-device", "virtio-rng-device", "-serial", "file:" + log, "-display", "none"]
        if hold_us:
            argv += ["-object", "filter-buffer,id=fb0,netdev=net0,queue=all,interval=%d" % hold_us]
        p = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if not wait_log(log, "boot: complete", 120):
            p.kill()
            sys.exit("sftp-bench: the kernel did not boot")
        time.sleep(2)
        return p

    def run(self, hold_us):
        p = self.boot(hold_us)
        try:
            sh = Shell()
            sh.run("wget http://10.0.2.2:%d/sshd.ko /sshd.ko" % HTTP_PORT)
            if "loaded" not in sh.run("insmod /sshd.ko"):
                sys.exit("sftp-bench: sshd.ko would not load")
            sh.run("sshd allow " + open(self.key + ".pub").read().strip())
            if "listening" not in sh.run("sshd start"):
                sys.exit("sftp-bench: sshd would not start")
            rtt = None
            out = sh.run("ping 10.0.2.2")
            times = [float(t) for t in re.findall(r"time[=<]?\s*([\d.]+)\s*ms", out)]
            if times:
                rtt = statistics.median(times)
            nothing = statistics.median(self.sftp(["pwd"]) for _ in range(3))
            back = os.path.join(self.tmp, "back.bin")
            up, down = [], []
            mb = self.args.mib * (1 << 20) / 1e6
            for _ in range(self.args.runs):
                up.append(mb / max(self.sftp(["put %s /bench.bin" % self.big]) - nothing, 1e-3))
                if os.path.exists(back):
                    os.remove(back)
                down.append(mb / max(self.sftp(["get /bench.bin %s" % back]) - nothing, 1e-3))
                with open(back, "rb") as f:
                    if hashlib.sha256(f.read()).hexdigest() != self.digest:
                        sys.exit("sftp-bench: the file came back different")
                self.sftp(["rm /bench.bin"])
            return rtt, nothing, up, down
        finally:
            p.send_signal(signal.SIGTERM)
            try:
                p.wait(10)
            except subprocess.TimeoutExpired:
                p.kill()


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--tree", default=ROOT, help="the built tree whose nos-arm64.img and sshd.ko are run")
    ap.add_argument("--mib", type=int, default=64, help="the file's size")
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--hold-us", type=int, action="append",
                    help="filter-buffer's interval, microseconds: 0 for none (default: 0 and 15000)")
    ap.add_argument("--tcg", action="store_true")
    args = ap.parse_args()
    args.tree = os.path.abspath(args.tree)
    holds = args.hold_us or [0, 15000]

    tmp = tempfile.mkdtemp(prefix="sftp-bench.")
    served = os.path.join(tmp, "served")
    os.makedirs(served)
    shutil.copy(os.path.join(args.tree, "out", "aarch64", "modules", "sshd.ko"), served)
    httpd = serve_http(served)
    try:
        b = Bench(args, tmp)
        print("sftp-bench: %s, %d MiB, %d runs" % (args.tree, args.mib, args.runs), flush=True)
        for hold in holds:
            rtt, nothing, up, down = b.run(hold)
            print("hold %6d us  rtt %s  session %.2f s  put MB/s %s median %.1f  get MB/s %s median %.1f" % (
                hold, "%.1f ms" % rtt if rtt is not None else "?", nothing,
                " ".join("%.1f" % x for x in up), statistics.median(up),
                " ".join("%.1f" % x for x in down), statistics.median(down)), flush=True)
    finally:
        httpd.shutdown()
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
