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

# Datagrams between reply drains, and the shortest gap worth sleeping for.
BURST = 32
MIN_SLEEP = 0.001


class Sender:
    def __init__(self, host, port, size, deadline, pace, pps):
        self.host = host
        self.port = port
        self.payload = b"L" * size
        self.deadline = deadline
        self.pace = pace
        # Datagrams a second this thread aims for; 0 sends as fast as it can.
        self.pps = pps
        self.sent = 0
        self.received = 0
        self.errors = 0
        # Seconds spent sending, which is not the same as the thread's
        # lifetime: the drain at the end would otherwise be counted against
        # the send rate and understate it by however long it took.
        self.send_seconds = 0.0

    def run(self):
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        # Non-blocking, not a short timeout: a blocking recv between bursts
        # caps the sender at burst/timeout datagrams a second no matter how
        # fast it could send -- at 32 per burst and 20 ms that is 1600 pps,
        # which is nowhere near what the sender or the target can do.
        sock.setblocking(False)
        try:
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 4 << 20)
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4 << 20)
        except OSError:
            pass

        started = time.time()
        try:
            while time.time() < self.deadline:
                for _ in range(BURST):
                    try:
                        sock.sendto(self.payload, (self.host, self.port))
                        self.sent += 1
                    except BlockingIOError:
                        pass
                    except OSError:
                        # ICMP port-unreachable comes back as an error on the
                        # next send; it means the target is not listening, not
                        # that the sender broke.
                        self.errors += 1

                self.drain(sock)

                if self.pps:
                    # Sleep only once the debt is worth a syscall: sleeping per
                    # datagram costs more than the gap it is meant to create,
                    # and the granularity of sleep then sets the rate instead
                    # of the argument.
                    due = started + self.sent / self.pps
                    behind = due - time.time()
                    if behind > MIN_SLEEP:
                        time.sleep(behind)

                if self.pace:
                    time.sleep(self.pace)

            self.send_seconds = time.time() - started

            # Drain whatever is still in flight.
            drain_until = time.time() + 0.5
            while time.time() < drain_until:
                if not self.drain(sock):
                    time.sleep(0.005)
        finally:
            sock.close()

    def drain(self, sock):
        """Take every reply waiting right now. Returns True if any arrived."""
        got = False
        while True:
            try:
                sock.recv(65535)
                self.received += 1
                got = True
            except (BlockingIOError, OSError):
                return got


def main():
    p = argparse.ArgumentParser(description="UDP load generator for the nos netload target")
    p.add_argument("host")
    p.add_argument("-p", "--port", type=int, default=9999)
    p.add_argument("-s", "--seconds", type=float, default=10.0)
    p.add_argument("-b", "--size", type=int, default=512,
                   help="payload bytes per datagram (default 512)")
    p.add_argument("-t", "--threads", type=int, default=1)
    p.add_argument("--pps", type=float, default=0.0,
                   help="datagrams a second to aim for, across all threads; "
                        "0 (the default) sends as fast as the sender can. "
                        "Worth setting: the interesting range for a NIC is "
                        "usually well below what a laptop can emit, and a "
                        "flat-out sender mostly measures the path in between")
    p.add_argument("--pace", type=float, default=0.0,
                   help="seconds to sleep between bursts of 32; 0 sends flat out")
    args = p.parse_args()

    if args.size < 1 or args.size > 1400:
        print("size must be 1..1400", file=sys.stderr)
        return 2

    deadline = time.time() + args.seconds
    per_thread_pps = (args.pps / args.threads) if args.pps else 0.0
    senders = [Sender(args.host, args.port, args.size, deadline, args.pace,
                      per_thread_pps)
               for _ in range(args.threads)]
    threads = [threading.Thread(target=s.run, daemon=True) for s in senders]

    print("sending to %s:%d for %.1fs, %d bytes, %d thread(s), %s"
          % (args.host, args.port, args.seconds, args.size, args.threads,
             ("%g pps" % args.pps) if args.pps else "flat out"))
    started = time.time()
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    elapsed = time.time() - started

    sent = sum(s.sent for s in senders)
    received = sum(s.received for s in senders)
    errors = sum(s.errors for s in senders)

    # Over the sending window, not the whole run: the drain at the end is not
    # time spent sending, and counting it makes every rate read low.
    window = max((s.send_seconds for s in senders), default=elapsed) or elapsed

    print("sent     %d (%.0f pps, %.1f Mbit/s)"
          % (sent, sent / window, sent * args.size * 8 / window / 1e6))
    print("echoed   %d (%.0f pps)" % (received, received / window))
    if sent:
        print("returned %.1f%%" % (100.0 * received / sent))
    if errors:
        print("send errors %d" % errors)
    print("\nnow ask the machine: `netload` for its own counters, and compare.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
