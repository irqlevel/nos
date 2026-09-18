#!/usr/bin/env python3
"""TCP test: work the kernel's TCP from the shell against a server this
script runs, and check both what came back and what the connection table
looks like afterwards.

What TCP had before this was the sshd test -- which is a good one, but it
exercises the listening side over one long-lived connection. This exercises
the connecting side and the edges: a transfer big enough to need many
segments and a window that moves, a connection refused, one to a black hole
that has to time out, and enough connections in a row to reuse ephemeral
ports through TIME-WAIT. The last check is the one that catches a slot leak:
after all of it the pool has to be back where it started, because a leak of
one slot per connection is invisible until the sixty-fourth.

    scripts/tcp-test.py [--tcg] [--keep]

arm64 only, because it drives the shell over UDP. Exit code 0 = every check
passed.
"""

import argparse
import hashlib
import http.server
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
HTTP_PORT = 8088

# Big enough to cross many segments and make the window move: with a 1460
# byte MSS this is over 250 of them, and more than the 8 KiB receive buffer
# holds, so the transfer only finishes if the window opens again as the
# kernel drains it.
BIG_SIZE = 400 * 1024

SMALL = b"tcp-test: a short body\n"


class Body(http.server.BaseHTTPRequestHandler):
    """Two resources: a short one and one worth streaming."""

    big = b""

    def do_GET(self):
        body = Body.big if self.path == "/big" else SMALL
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Content-Type", "application/octet-stream")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


def serve(port):
    Body.big = bytes((i * 7 + 13) & 0xFF for i in range(BIG_SIZE))
    server = http.server.ThreadingHTTPServer(("0.0.0.0", port), Body)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, hashlib.sha256(Body.big).hexdigest()


def conn_counts(sh):
    """What `tcpstat` says: (connections listed, established)."""
    out = sh.run("tcpstat")
    listed = 0
    established = 0
    for line in out.splitlines():
        low = line.lower()
        if "established" in low:
            established += 1
        # Connection lines carry a state name; the summary lines do not.
        if any(s in line for s in ("ESTABLISHED", "TIME-WAIT", "SYN-SENT",
                                   "CLOSE-WAIT", "LAST-ACK", "FIN-WAIT")):
            listed += 1
    return listed, established, out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tcg", action="store_true")
    ap.add_argument("--keep", action="store_true")
    ap.add_argument("--nic", choices=["virtio", "igb"], default="virtio",
                    help="the network card: igb is QEMU's 82576, which the Rust igb driver claims")
    args = ap.parse_args()

    tmp = tempfile.mkdtemp(prefix="nos-tcp-")
    image = os.path.join(tmp, "root.img")
    log = os.path.join(tmp, "serial.log")
    pt.mkrootfs(image)

    server, want_sha = serve(HTTP_PORT)

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
        "-netdev", "user,id=n0,hostfwd=udp::%d-:%d" % (SHELL_PORT, SHELL_PORT),
        "-serial", "file:" + log, "-display", "none"]

    p = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        if not pt.check("reaches the shell", pt.wait_log(log, "boot: complete", 300)):
            return 1
        time.sleep(12)   # let the boot-time DHCP finish
        sh = pt.Shell()
        sh.sock.settimeout(60)

        base = "http://10.0.2.2:%d" % HTTP_PORT
        before, _, _ = conn_counts(sh)

        out = sh.run("wget %s/small /small.txt" % base)
        pt.check("a short body comes back whole",
                 "HTTP 200" in out and str(len(SMALL)) in out, out)
        pt.check("and is what the server sent",
                 SMALL.decode().strip() in sh.run("cat /small.txt"))

        out = sh.run("wget %s/big /big.bin" % base)
        pt.check("a body of %d bytes comes back too" % BIG_SIZE,
                 "HTTP 200" in out and str(BIG_SIZE) in out, out)
        out = sh.run("stat /big.bin")
        pt.check("with every byte of it",
                 ("%d bytes" % BIG_SIZE) in out, out)
        out = sh.run("sha256 /big.bin")
        pt.check("and the bytes are the ones sent",
                 want_sha in out.replace(" ", "").lower(), out)

        # A port nothing listens on: the connect has to be refused, not hang.
        closed = 9
        start = time.time()
        out = sh.run("wget http://10.0.2.2:%d/ /nope.txt" % closed)
        refused = time.time() - start
        pt.check("a connection nothing listens on is refused",
                 "HTTP 200" not in out, out)
        pt.check("and refused promptly rather than waiting out a timeout",
                 refused < 20, "%.1f s" % refused)

        # A black hole: no SYN-ACK, no RST. This one has to time out.
        start = time.time()
        out = sh.run("wget http://10.0.0.254:1/ /nope.txt")
        timed_out = time.time() - start
        pt.check("a connection nothing answers times out",
                 "HTTP 200" not in out, out)
        pt.check("within a bounded time", timed_out < 60, "%.1f s" % timed_out)

        # Enough connections in a row to walk the ephemeral range and reuse
        # ports whose old incarnations are still in TIME-WAIT.
        ok = 0
        for _ in range(12):
            if "HTTP 200" in sh.run("wget %s/small /s.txt" % base):
                ok += 1
        pt.check("twelve connections in a row all complete", ok == 12, "%d of 12" % ok)

        # A slot leaked per connection is invisible until the pool runs out.
        time.sleep(3)
        after, established, out = conn_counts(sh)
        pt.check("no connection is left established", established == 0, out)
        pt.check("and the table has not grown without bound",
                 after <= before + 16, "%d before, %d after\n%s" % (before, after, out))

        # Everything above went through the same stack the shell is on.
        pt.check("the shell still answers after all of it",
                 "eth0" in sh.run("net"))
    finally:
        server.shutdown()
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
    if pt.failures:
        print("FAILED: " + ", ".join(pt.failures))
        return 1
    print("tcp-test: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
