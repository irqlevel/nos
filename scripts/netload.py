#!/usr/bin/env python3
"""Drive the kernel's `netload` UDP target and report what came back.

Start the server on the machine under test first:

    netload start 9999          # echo mode; `netload start 9999 sink` to drop

then, from here:

    python3 scripts/netload.py 65.108.38.184 --seconds 20 --threads 4

and take a `profile 2000` over the shell while this runs -- that is the whole
point of it. Loss is expected and is not by itself a fault: UDP has no flow
control, and a target that answers every datagram is sending as hard as it is
receiving.
"""

import argparse
import socket
import sys
import threading
import time


class Sender:
    def __init__(self, host, port, size, deadline, pace):
        self.host = host
        self.port = port
        self.payload = b"L" * size
        self.deadline = deadline
        self.pace = pace
        self.sent = 0
        self.received = 0
        self.errors = 0

    def run(self):
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.settimeout(0.02)
        try:
            while time.time() < self.deadline:
                for _ in range(32):
                    try:
                        sock.sendto(self.payload, (self.host, self.port))
                        self.sent += 1
                    except OSError:
                        self.errors += 1
                try:
                    while True:
                        sock.recv(65535)
                        self.received += 1
                except OSError:
                    pass
                if self.pace:
                    time.sleep(self.pace)

            # Drain whatever is still in flight.
            drain = time.time() + 0.5
            while time.time() < drain:
                try:
                    sock.recv(65535)
                    self.received += 1
                except OSError:
                    break
        finally:
            sock.close()


def main():
    p = argparse.ArgumentParser(description="UDP load generator for the nos netload target")
    p.add_argument("host")
    p.add_argument("-p", "--port", type=int, default=9999)
    p.add_argument("-s", "--seconds", type=float, default=10.0)
    p.add_argument("-b", "--size", type=int, default=512,
                   help="payload bytes per datagram (default 512)")
    p.add_argument("-t", "--threads", type=int, default=1)
    p.add_argument("--pace", type=float, default=0.0,
                   help="seconds to sleep between bursts of 32; 0 sends flat out")
    args = p.parse_args()

    if args.size < 1 or args.size > 1400:
        print("size must be 1..1400", file=sys.stderr)
        return 2

    deadline = time.time() + args.seconds
    senders = [Sender(args.host, args.port, args.size, deadline, args.pace)
               for _ in range(args.threads)]
    threads = [threading.Thread(target=s.run, daemon=True) for s in senders]

    print("sending to %s:%d for %.1fs, %d bytes, %d thread(s)"
          % (args.host, args.port, args.seconds, args.size, args.threads))
    started = time.time()
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    elapsed = time.time() - started

    sent = sum(s.sent for s in senders)
    received = sum(s.received for s in senders)
    errors = sum(s.errors for s in senders)

    print("sent     %d (%.0f pps, %.1f Mbit/s)"
          % (sent, sent / elapsed, sent * args.size * 8 / elapsed / 1e6))
    print("echoed   %d (%.0f pps)" % (received, received / elapsed))
    if sent:
        print("returned %.1f%%" % (100.0 * received / sent))
    if errors:
        print("send errors %d" % errors)
    print("\nnow ask the machine: `netload` for its own counters, and compare.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
