#!/usr/bin/env python3
"""Netconsole collector for the nos kernel.

The kernel streams its log over UDP when booted with `netconsole=ip:port`
(see build/grub.cfg on x86-64, -append on arm64). Each datagram carries one or
more whole log lines as plain text; this script binds the port, reassembles
lines per sender and prints them, optionally to a file as well.

    ./scripts/netconsole.py                 # listen on 0.0.0.0:6666
    ./scripts/netconsole.py -p 5555 -o boot.log
    ./scripts/netconsole.py --from 10.0.2.15 --no-stamp

Kernel side, e.g. in build/grub.cfg:

    multiboot2 /boot/kernel64.elf netconsole=10.0.2.2:6666 dhcp=auto

Under QEMU user networking the host is 10.0.2.2, and the port has to be
forwarded into the guest network only for the reverse direction -- outbound
UDP from the guest reaches the host without any -netdev option, so
`-netdev user,id=n0` plus `netconsole=10.0.2.2:6666` is enough.
"""

import argparse
import errno
import os
import signal
import socket
import sys
import time

DEFAULT_PORT = 6666
DEFAULT_BIND = "0.0.0.0"

# Flush a sender's partial line if nothing more arrives within this window, so
# a panic message that never got its newline still shows up.
PARTIAL_TIMEOUT = 0.5


class Sink:
    """Line-oriented output: stdout plus an optional log file."""

    def __init__(self, path, stamp, show_source, multi_source):
        self.stamp = stamp
        self.show_source = show_source
        self.multi_source = multi_source
        self.file = open(path, "a", buffering=1) if path else None
        self.start = time.time()

    def write_line(self, source, line):
        prefix = ""
        if self.stamp:
            prefix += "%8.3f " % (time.time() - self.start)
        if self.show_source or self.multi_source:
            prefix += "%s:%d " % source
        text = prefix + line
        print(text, flush=True)
        if self.file:
            self.file.write(text + "\n")

    def close(self):
        if self.file:
            self.file.close()
            self.file = None


class Reassembler:
    """Per-sender partial-line buffer."""

    def __init__(self, sink):
        self.sink = sink
        self.partial = {}
        self.last_seen = {}

    def feed(self, source, data):
        text = self.partial.pop(source, "") + data.decode("utf-8", errors="replace")
        lines = text.split("\n")
        self.partial[source] = lines.pop()
        self.last_seen[source] = time.time()
        for line in lines:
            self.sink.write_line(source, line.rstrip("\r"))

    def flush_stale(self, now):
        for source in list(self.partial):
            rest = self.partial[source]
            if rest and now - self.last_seen.get(source, 0) > PARTIAL_TIMEOUT:
                self.partial[source] = ""
                self.sink.write_line(source, rest.rstrip("\r"))

    def flush_all(self):
        for source in list(self.partial):
            rest = self.partial.pop(source)
            if rest:
                self.sink.write_line(source, rest.rstrip("\r"))


def on_sigterm(signum, frame):
    """Leave via the same path as ^C, so buffers are flushed."""
    raise KeyboardInterrupt


def main():
    signal.signal(signal.SIGTERM, on_sigterm)

    parser = argparse.ArgumentParser(
        description="Receive the nos kernel log sent by netconsole=ip:port")
    parser.add_argument("-p", "--port", type=int, default=DEFAULT_PORT,
                        help="UDP port to listen on (default %d)" % DEFAULT_PORT)
    parser.add_argument("-b", "--bind", default=DEFAULT_BIND,
                        help="address to bind (default %s)" % DEFAULT_BIND)
    parser.add_argument("-o", "--output", metavar="FILE",
                        help="also append every line to FILE")
    parser.add_argument("--from", dest="source", metavar="IP",
                        help="only show lines from this sender")
    parser.add_argument("--no-stamp", action="store_true",
                        help="do not prefix lines with the host receive time")
    parser.add_argument("--show-source", action="store_true",
                        help="always prefix lines with the sender ip:port")
    args = parser.parse_args()

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        sock.bind((args.bind, args.port))
    except OSError as e:
        if e.errno == errno.EACCES:
            sys.exit("cannot bind %s:%d: permission denied (ports below 1024 "
                     "need root)" % (args.bind, args.port))
        sys.exit("cannot bind %s:%d: %s" % (args.bind, args.port, e.strerror))
    sock.settimeout(PARTIAL_TIMEOUT)

    sink = Sink(args.output, not args.no_stamp, args.show_source,
                multi_source=False)
    asm = Reassembler(sink)
    seen_sources = set()
    packets = 0
    total = 0

    print("netconsole: listening on %s:%d%s" %
          (args.bind, args.port,
           (", logging to " + args.output) if args.output else ""),
          file=sys.stderr)

    try:
        while True:
            try:
                data, source = sock.recvfrom(65535)
            except socket.timeout:
                asm.flush_stale(time.time())
                continue

            if args.source and source[0] != args.source:
                continue

            if source not in seen_sources:
                seen_sources.add(source)
                if len(seen_sources) > 1:
                    sink.multi_source = True
                print("netconsole: sender %s:%d" % source, file=sys.stderr)

            packets += 1
            total += len(data)
            asm.feed(source, data)
    except KeyboardInterrupt:
        pass
    finally:
        asm.flush_all()
        sink.close()
        sock.close()
        print("\nnetconsole: %d packets, %d bytes from %d sender(s)" %
              (packets, total, len(seen_sources)), file=sys.stderr)


if __name__ == "__main__":
    if os.name != "posix":
        print("warning: only tested on posix hosts", file=sys.stderr)
    main()
