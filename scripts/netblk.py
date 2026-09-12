#!/usr/bin/env python3
"""Client for netblk, the nos module that serves a disk over UDP.

    netblk.py HOST PORT info
    netblk.py HOST PORT read OFFSET LENGTH [FILE]
    netblk.py HOST PORT write OFFSET FILE [--fua]
    netblk.py HOST PORT flush
    netblk.py HOST PORT verify [--offset N] [--size N]      destroys what is there
    netblk.py HOST PORT bench [--mode randread|randwrite|read|write]
                              [--secs N] [--size N] [--window N]

Sizes take k, m and g. Requests are pipelined -- up to --window at once --
and retried when an answer does not come back in time; every one of them is
idempotent, so a retry never needs to know whether the first try arrived.
The protocol is in docs/netblk.md.
"""

import argparse
import hashlib
import os
import random
import select
import socket
import struct
import sys
import time

MAGIC = 0x4E424C4B  # "NBLK"
VERSION = 1

OP_INFO, OP_READ, OP_WRITE, OP_FLUSH = 1, 2, 3, 4
OP_REPLY = 0x80
FLAG_FUA = 1

ST_OK, ST_BADREQ, ST_RANGE, ST_IO, ST_BUSY, ST_ROFS = range(6)
STATUS = {
    ST_OK: "ok",
    ST_BADREQ: "bad request",
    ST_RANGE: "out of range",
    ST_IO: "I/O error",
    ST_BUSY: "busy",
    ST_ROFS: "read-only",
}

# magic, version, op, flags, cookie, offset, length, status
HDR = struct.Struct("!IBBHQQIH")
assert HDR.size == 30
# size, sector size, max_io, max_read, flags, slots, reserved
INFO = struct.Struct("!QIIIIII")
assert INFO.size == 32
INFO_READ_ONLY = 1

# A busy server is asked again this much later, this many times at most
BUSY_RETRY = 0.002
MAX_BUSY = 5000


class NetblkError(Exception):
    pass


def parse_size(text):
    units = {"k": 1 << 10, "m": 1 << 20, "g": 1 << 30}
    text = text.strip().lower()
    if text and text[-1] in units:
        return int(text[:-1], 0) * units[text[-1]]
    return int(text, 0)


def positive(text):
    n = int(text, 0)
    if n < 1:
        raise argparse.ArgumentTypeError("at least 1")
    return n


def human(n):
    for unit, name in ((1 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")):
        if n >= unit:
            return f"{n / unit:.1f} {name}"
    return f"{n} bytes"


class Request:
    __slots__ = ("op", "offset", "length", "data", "flags", "cookie",
                 "sent", "tries", "busy", "pieces", "got", "buf", "started", "finished")

    def __init__(self, op, offset=0, length=0, data=b"", flags=0):
        self.op = op
        self.offset = offset
        self.length = length
        self.data = data
        self.flags = flags
        self.cookie = 0
        self.sent = 0.0
        self.tries = 0
        self.busy = 0
        self.pieces = set()     # a read's answers so far, by offset
        self.got = 0
        self.buf = None
        self.started = 0.0
        self.finished = 0.0


class Client:
    def __init__(self, host, port, timeout=0.25, retries=40):
        self.addr = (socket.gethostbyname(host), port)
        self.timeout = timeout
        self.retries = retries
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        for opt in (socket.SO_RCVBUF, socket.SO_SNDBUF):
            try:
                self.sock.setsockopt(socket.SOL_SOCKET, opt, 8 << 20)
            except OSError:
                pass
        self.sock.connect(self.addr)
        self.sock.setblocking(False)
        self.cookie = random.getrandbits(31) << 32
        self.rxbuf = bytearray(65536)
        self.retransmits = 0
        self.busy = 0
        self.info()

    def _send(self, req):
        hdr = HDR.pack(MAGIC, VERSION, req.op, req.flags, req.cookie,
                       req.offset, req.length, 0)
        try:
            self.sock.send(hdr + req.data if req.data else hdr)
        except BlockingIOError:
            pass  # the retry timer covers it
        except ConnectionRefusedError:
            raise NetblkError(f"nothing listens on {self.addr[0]}:{self.addr[1]}")
        req.sent = time.monotonic()
        req.tries += 1

    def _replies(self, wait):
        """Every reply waiting now, after waiting up to `wait` for the first."""
        ready, _, _ = select.select([self.sock], [], [], max(wait, 0))
        if not ready:
            return
        view = memoryview(self.rxbuf)
        while True:
            try:
                n = self.sock.recv_into(self.rxbuf)
            except BlockingIOError:
                return
            except ConnectionRefusedError:
                raise NetblkError(f"nothing listens on {self.addr[0]}:{self.addr[1]}")
            if n < HDR.size:
                continue
            magic, version, op, _flags, cookie, offset, length, status = HDR.unpack_from(self.rxbuf)
            if magic != MAGIC or version != VERSION or not op & OP_REPLY:
                continue
            yield op & ~OP_REPLY, cookie, offset, length, status, view[HDR.size:n]

    def run(self, requests, window, on_done=None):
        """Every request in `requests` answered, `window` of them in flight at
        once; on_done(req) as each completes."""
        pending = iter(requests)
        inflight = {}
        exhausted = False

        while True:
            while not exhausted and len(inflight) < window:
                req = next(pending, None)
                if req is None:
                    exhausted = True
                    break
                self.cookie += 1
                req.cookie = self.cookie
                req.started = time.monotonic()
                if req.op == OP_READ:
                    req.buf = bytearray(req.length)
                self._send(req)
                inflight[req.cookie] = req

            if not inflight:
                return

            now = time.monotonic()
            first = min(r.sent for r in inflight.values())
            for op, cookie, offset, length, status, payload in self._replies(first + self.timeout - now):
                req = inflight.get(cookie)
                if req is None or op != req.op:
                    continue  # an answer to a retry already answered
                if status == ST_BUSY:
                    # Not a lost request: asked again shortly, and not counted
                    # against the retries a silent server gets -- a server
                    # that stays busy has a limit of its own
                    self.busy += 1
                    req.busy += 1
                    if req.busy > MAX_BUSY:
                        raise NetblkError(f"the server stayed busy through {MAX_BUSY} tries")
                    req.tries -= 1
                    req.sent = time.monotonic() - self.timeout + BUSY_RETRY
                    continue
                if status != ST_OK:
                    raise NetblkError(f"{STATUS.get(status, status)} at offset {offset}, "
                                      f"{length} bytes")
                if req.op == OP_READ:
                    if offset in req.pieces:
                        continue
                    at = offset - req.offset
                    if at < 0 or at + len(payload) > req.length or len(payload) != length:
                        raise NetblkError(f"a read answered with {len(payload)} bytes at {offset}")
                    req.buf[at:at + length] = payload
                    req.pieces.add(offset)
                    req.got += length
                    if req.got < req.length:
                        continue
                del inflight[cookie]
                req.finished = time.monotonic()
                if on_done:
                    on_done(req)

            now = time.monotonic()
            for req in inflight.values():
                if now - req.sent >= self.timeout:
                    if req.tries > self.retries:
                        raise NetblkError(f"no answer from {self.addr[0]}:{self.addr[1]} "
                                          f"after {req.tries} tries")
                    self.retransmits += 1
                    self._send(req)

    def info(self):
        """The disk's geometry and the server's limits, which every other
        request is sized by"""
        self.cookie += 1
        req = Request(OP_INFO)
        req.cookie = self.cookie
        self._send(req)

        payload = None
        while payload is None:
            if time.monotonic() - req.sent >= self.timeout:
                if req.tries > self.retries:
                    raise NetblkError(f"no answer from {self.addr[0]}:{self.addr[1]}")
                self._send(req)
            wait = req.sent + self.timeout - time.monotonic()
            for op, cookie, _offset, _length, status, data in self._replies(wait):
                if op != OP_INFO or cookie != req.cookie:
                    continue
                if status != ST_OK or len(data) < INFO.size:
                    raise NetblkError(f"INFO answered {STATUS.get(status, status)}")
                payload = bytes(data[:INFO.size])
                break

        (self.size, self.sector_size, self.max_io, self.max_read,
         flags, self.slots, _) = INFO.unpack(payload)
        self.read_only = bool(flags & INFO_READ_ONLY)

    def reads(self, offset, length):
        """READ requests covering [offset, offset + length), max_read each"""
        end = offset + length
        while offset < end:
            n = min(self.max_read, end - offset)
            yield Request(OP_READ, offset, n)
            offset += n

    def writes(self, offset, data, flags=0):
        """WRITE requests carrying `data` from `offset`, max_io each"""
        view = memoryview(data)
        for at in range(0, len(data), self.max_io):
            chunk = bytes(view[at:at + self.max_io])
            yield Request(OP_WRITE, offset + at, len(chunk), chunk, flags)

    def read_window(self, window, length):
        """A window of reads `length` bytes each the server has slots for:
        a read takes one for every datagram of its answer, and a request past
        them is refused busy."""
        pieces = max(1, -(-min(length, self.max_read) // self.max_io))
        return max(1, min(window, self.slots // pieces))

    def read(self, offset, length, window):
        out = bytearray(length)

        def done(req):
            at = req.offset - offset
            out[at:at + req.length] = req.buf

        self.run(self.reads(offset, length), self.read_window(window, length), done)
        return out

    def write(self, offset, data, window, flags=0):
        self.run(self.writes(offset, data, flags), window)

    def flush(self):
        self.run([Request(OP_FLUSH)], 1)


def check_range(client, offset, length):
    sector = client.sector_size
    if offset % sector or length % sector:
        raise NetblkError(f"offset and length must be multiples of {sector}")
    if offset + length > client.size:
        raise NetblkError(f"past the end: the disk is {client.size} bytes")


def cmd_info(client, args):
    print(f"size {client.size} bytes ({human(client.size)}), "
          f"sector {client.sector_size} bytes")
    print(f"{client.max_io} bytes a datagram, reads of up to {client.max_read}, "
          f"{client.slots} requests in flight at most")
    print("read-only" if client.read_only else "read-write")


def cmd_read(client, args):
    offset, length = parse_size(args.offset), parse_size(args.length)
    check_range(client, offset, length)
    start = time.monotonic()
    data = client.read(offset, length, args.window)
    took = time.monotonic() - start
    if args.file:
        with open(args.file, "wb") as f:
            f.write(data)
    print(f"read {human(length)} in {took:.2f} s ({length / took / (1 << 20):.1f} MiB/s), "
          f"sha256 {hashlib.sha256(data).hexdigest()}")


def cmd_write(client, args):
    offset = parse_size(args.offset)
    with open(args.file, "rb") as f:
        data = f.read()
    check_range(client, offset, len(data))
    start = time.monotonic()
    client.write(offset, data, args.window, FLAG_FUA if args.fua else 0)
    client.flush()
    took = time.monotonic() - start
    print(f"wrote {human(len(data))} in {took:.2f} s "
          f"({len(data) / took / (1 << 20):.1f} MiB/s), flushed")


def cmd_flush(client, args):
    client.flush()
    print("flushed")


def cmd_verify(client, args):
    offset = parse_size(args.offset)
    size = parse_size(args.size) if args.size else min(client.size - offset, 4 << 20)
    check_range(client, offset, size)
    if size == 0:
        raise NetblkError("nothing to verify there")
    if client.read_only:
        raise NetblkError("the server is read-only")

    data = random.Random(args.seed).randbytes(size)
    start = time.monotonic()
    client.write(offset, data, args.window)
    client.flush()
    wrote = time.monotonic() - start

    start = time.monotonic()
    back = client.read(offset, size, args.window)
    read = time.monotonic() - start

    mib = size / (1 << 20)
    print(f"wrote {human(size)} at {offset} in {wrote:.2f} s ({mib / wrote:.1f} MiB/s), "
          f"read it back in {read:.2f} s ({mib / read:.1f} MiB/s)")
    if back != data:
        bad = next(i for i in range(size) if back[i] != data[i])
        raise NetblkError(f"MISMATCH: first differing byte at {offset + bad}")
    print(f"verified: identical, sha256 {hashlib.sha256(data).hexdigest()[:16]}...; "
          f"{client.retransmits} retransmits, {client.busy} busy")


def cmd_bench(client, args):
    write = args.mode in ("write", "randwrite")
    rand = args.mode.startswith("rand")
    if write and client.read_only:
        raise NetblkError("the server is read-only")

    size = parse_size(args.size) if args.size else client.size
    size = min(size, client.size)
    bs = client.max_io if write else (parse_size(args.bs) if args.bs else client.max_io)
    bs = min(bs, client.max_read)
    bs -= bs % client.sector_size
    if bs == 0:
        raise NetblkError(f"bs is at least a sector, {client.sector_size} bytes")
    blocks = size // bs
    if blocks == 0:
        raise NetblkError("nothing to test over")

    payload = os.urandom(bs)
    deadline = time.monotonic() + args.secs
    rng = random.Random(args.seed)
    cursor = [0]

    def requests():
        while time.monotonic() < deadline:
            block = rng.randrange(blocks) if rand else cursor[0] % blocks
            cursor[0] += 1
            if write:
                yield Request(OP_WRITE, block * bs, bs, payload)
            else:
                yield Request(OP_READ, block * bs, bs)

    latencies = []

    def done(req):
        latencies.append(req.finished - req.started)

    window = args.window if write else client.read_window(args.window, bs)
    start = time.monotonic()
    client.run(requests(), window, done)
    took = time.monotonic() - start
    if write:
        client.flush()

    n = len(latencies)
    if n == 0:
        raise NetblkError("nothing completed")
    latencies.sort()
    pct = lambda p: latencies[min(n - 1, int(n * p))] * 1e6
    print(f"{args.mode}, bs {bs}, window {window}: {n} ios in {took:.2f} s, "
          f"{n / took:.0f} IOPS, {n * bs / took / (1 << 20):.1f} MiB/s")
    print(f"latency us: avg {sum(latencies) / n * 1e6:.0f}, p50 {pct(0.5):.0f}, "
          f"p99 {pct(0.99):.0f}, max {latencies[-1] * 1e6:.0f}; "
          f"{client.retransmits} retransmits, {client.busy} busy")


def main():
    p = argparse.ArgumentParser(description="netblk client (docs/netblk.md)")
    p.add_argument("host")
    p.add_argument("port", type=int)
    p.add_argument("--window", type=positive, default=32, help="requests in flight (32)")
    p.add_argument("--timeout", type=float, default=0.25, help="seconds before a retry")
    sub = p.add_subparsers(dest="cmd", required=True)

    sub.add_parser("info")
    s = sub.add_parser("read")
    s.add_argument("offset")
    s.add_argument("length")
    s.add_argument("file", nargs="?")
    s = sub.add_parser("write")
    s.add_argument("offset")
    s.add_argument("file")
    s.add_argument("--fua", action="store_true")
    sub.add_parser("flush")
    s = sub.add_parser("verify")
    s.add_argument("--offset", default="0")
    s.add_argument("--size")
    s.add_argument("--seed", type=int, default=1)
    s = sub.add_parser("bench")
    s.add_argument("--mode", default="randread",
                   choices=["randread", "randwrite", "read", "write"])
    s.add_argument("--secs", type=float, default=5)
    s.add_argument("--size")
    s.add_argument("--bs")
    s.add_argument("--seed", type=int, default=1)

    args = p.parse_args()
    try:
        client = Client(args.host, args.port, timeout=args.timeout)
        {"info": cmd_info, "read": cmd_read, "write": cmd_write, "flush": cmd_flush,
         "verify": cmd_verify, "bench": cmd_bench}[args.cmd](client, args)
    except NetblkError as e:
        print(f"netblk: {e}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
