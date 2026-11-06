#!/usr/bin/env python3
"""A LAN for QEMUs: a learning Ethernet switch over their UDP netdevs, which
answers DHCP itself and drops, delays and cuts what it carries -- the
network of plans/ipc.md's I4 tests (ipc-test.py), and of NBS's after them.

Each port is two UDP sockets on 127.0.0.1, the hub's -- which its QEMU sends
frames to -- and the QEMU's own, which the hub sends to:

    -netdev dgram,id=lan,local.type=inet,local.host=127.0.0.1,local.port=<qemu>,
            remote.type=inet,remote.host=127.0.0.1,remote.port=<hub>

(`Hub.netdev(i)` says it, and `Hub.mac(i)` the MAC its NIC is to have.) A
DHCP request from a port's MAC is answered with that port's address,
10.9.0.<i+1>/24, the hub's own 10.9.0.254 its server and its router; ARP
for the hub's addresses is answered too. Everything else is switched:
learned by source MAC, flooded while the destination is not known yet.

A test reaches a VM's UDP shell through the hub, whatever is cut: what it
sends to `Hub.shell(i)` on 127.0.0.1 goes to the VM's shell port from the
hub's 10.9.0.253, and the VM's answers come back -- past every impairment,
which is the VMs' between themselves.

Between any two ports, either way, while it runs: `cut(a, b)` drops every
frame and `heal(a, b)` stops that; `impair(a, b, loss=, delay_ms=,
jitter_ms=)` loses `loss` frames in 256 and delays each by the delay and up
to the jitter more -- which reorders what goes close together. `drop_next
(a, b, n)` loses the next n frames that carry data -- a TCP segment's tail,
say -- and nothing else.

    scripts/hub.py --ports 3 --base 47000       run one, printing each port's netdev
"""

import argparse
import heapq
import random
import select
import socket
import struct
import threading
import time

NET = "10.9.0."
HUB_IP = NET + "254"
SHELL_IP = NET + "253"
SHELL_SPORT = 40000
HUB_MAC = bytes.fromhex("020000090000")
MASK = "255.255.255.0"
LEASE_SECS = 86400
BROADCAST = b"\xff" * 6
ETH_IP = 0x0800
ETH_ARP = 0x0806
UDP = 17
TCP = 6
DHCP_SERVER_PORT = 67
DHCP_CLIENT_PORT = 68
DHCP_MAGIC = b"\x63\x82\x53\x63"
DHCP_DISCOVER, DHCP_OFFER, DHCP_REQUEST, DHCP_ACK = 1, 2, 3, 5


def ip_bytes(text):
    return socket.inet_aton(text)


def checksum(data):
    if len(data) % 2:
        data += b"\0"
    s = sum(struct.unpack("!%dH" % (len(data) // 2), data))
    while s >> 16:
        s = (s & 0xFFFF) + (s >> 16)
    return (~s) & 0xFFFF


def ipv4(src, dst, proto, payload):
    head = struct.pack("!BBHHHBBH4s4s", 0x45, 0, 20 + len(payload), 0, 0, 64, proto, 0, src, dst)
    head = head[:10] + struct.pack("!H", checksum(head)) + head[12:]
    return head + payload


def udp(src_ip, dst_ip, sport, dport, payload):
    head = struct.pack("!HHHH", sport, dport, 8 + len(payload), 0)
    pseudo = src_ip + dst_ip + struct.pack("!BBH", 0, UDP, 8 + len(payload))
    c = checksum(pseudo + head + payload) or 0xFFFF
    return head[:6] + struct.pack("!H", c) + payload


class Port:
    def __init__(self, index, hub_port, qemu_port):
        self.index = index
        self.hub_port = hub_port
        self.qemu_port = qemu_port
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.bind(("127.0.0.1", hub_port))
        self.sock.setblocking(False)
        self.frames_in = 0
        self.frames_out = 0
        # Frames to this port's QEMU the host would not take -- a send buffer
        # or the loopback's queue full -- by errno: lost here, and counted,
        # where they once went without a word
        self.send_failed = {}
        # The TCP segments this port's QEMU sent, by how much data each
        # carried: none (an acknowledgement), under 100 bytes, under 1400,
        # and full -- the mix of frames a transfer cost
        self.segments = {"ack": 0, "tiny": 0, "mid": 0, "full": 0}


class Link:
    """What happens to frames from one port to another."""

    def __init__(self):
        self.cut = False
        self.loss = 0
        self.delay = 0.0
        self.jitter = 0.0
        self.drop_data = 0
        self.dropped = 0


class Hub:
    def __init__(self, ports, base, seed=1, shell_port=0):
        self.ports = [Port(i, base + 2 * i, base + 2 * i + 1) for i in range(ports)]
        self.shell_port = shell_port
        self.shells = []
        self.shell_peer = {}
        for i in range(ports):
            s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            s.bind(("127.0.0.1", base + 100 + i))
            s.setblocking(False)
            self.shells.append(s)
        self.links = {(a, b): Link() for a in range(ports) for b in range(ports) if a != b}
        self.macs = {}
        self.rng = random.Random(seed)
        self.lock = threading.Lock()
        self.later = []
        self.seq = 0
        self.stop = False
        self.dhcp_answers = 0
        # The ports whose DHCP the hub has acknowledged: their VMs have
        # their addresses
        self.acked = set()
        self.thread = threading.Thread(target=self.serve, daemon=True)

    # -- what a test asks of it --

    def netdev(self, i, name="lan"):
        p = self.ports[i]
        return ("dgram,id=%s,local.type=inet,local.host=127.0.0.1,local.port=%d,"
                "remote.type=inet,remote.host=127.0.0.1,remote.port=%d" % (name, p.qemu_port, p.hub_port))

    def shell(self, i):
        """Where on 127.0.0.1 a test sends to port i's VM's UDP shell."""
        return self.shells[i].getsockname()[1]

    @staticmethod
    def mac(i):
        return "02:00:00:09:00:%02x" % (i + 1)

    @staticmethod
    def ip(i):
        return NET + str(i + 1)

    def start(self):
        self.thread.start()
        return self

    def close(self):
        self.stop = True
        self.thread.join(2)
        for p in self.ports:
            p.sock.close()
        for s in self.shells:
            s.close()

    def _links(self, a, b, both):
        return [self.links[(a, b)]] + ([self.links[(b, a)]] if both else [])

    def cut(self, a, b, both=True):
        with self.lock:
            for l in self._links(a, b, both):
                l.cut = True

    def heal(self, a, b, both=True):
        with self.lock:
            for l in self._links(a, b, both):
                l.cut = False

    def impair(self, a, b, loss=0, delay_ms=0.0, jitter_ms=0.0, both=True):
        with self.lock:
            for l in self._links(a, b, both):
                l.loss, l.delay, l.jitter = loss, delay_ms / 1000.0, jitter_ms / 1000.0

    def drop_next(self, a, b, n, both=False):
        with self.lock:
            for l in self._links(a, b, both):
                l.drop_data += n

    def dropped(self, a, b):
        with self.lock:
            return self.links[(a, b)].dropped

    # -- the switch --

    def send_to(self, i, frame):
        p = self.ports[i]
        try:
            p.sock.sendto(frame, ("127.0.0.1", p.qemu_port))
            p.frames_out += 1
        except OSError as e:
            p.send_failed[e.errno] = p.send_failed.get(e.errno, 0) + 1

    def tcp_data(self, frame):
        """How many bytes of data a TCP segment carries -- None for a frame
        that is no TCP segment."""
        if len(frame) < 14 + 20 or struct.unpack("!H", frame[12:14])[0] != ETH_IP or frame[23] != TCP:
            return None
        ihl = (frame[14] & 0x0F) * 4
        total = struct.unpack("!H", frame[16:18])[0]
        if len(frame) < 14 + ihl + 20:
            return None
        offset = (frame[14 + ihl + 12] >> 4) * 4
        return max(total - ihl - offset, 0)

    def carries_data(self, frame):
        """A TCP segment with data in it: what drop_next drops."""
        return (self.tcp_data(frame) or 0) > 0

    def forward(self, src, dst, frame, now):
        with self.lock:
            l = self.links[(src, dst)]
            if l.cut:
                l.dropped += 1
                return
            if l.drop_data and self.carries_data(frame):
                l.drop_data -= 1
                l.dropped += 1
                return
            if l.loss and self.rng.randrange(256) < l.loss:
                l.dropped += 1
                return
            delay = l.delay + (self.rng.random() * l.jitter if l.jitter else 0.0)
        if delay <= 0:
            self.send_to(dst, frame)
            return
        self.seq += 1
        heapq.heappush(self.later, (now + delay, self.seq, dst, frame))

    def dhcp(self, src, frame):
        """A DHCP request from port `src`, answered: True when it was one."""
        if len(frame) < 14 + 20 + 8 + 240 or struct.unpack("!H", frame[12:14])[0] != ETH_IP or frame[23] != UDP:
            return False
        ihl = (frame[14] & 0x0F) * 4
        u = 14 + ihl
        sport, dport = struct.unpack("!HH", frame[u:u + 4])
        if dport != DHCP_SERVER_PORT or sport != DHCP_CLIENT_PORT:
            return False
        b = frame[u + 8:]
        if len(b) < 240 or b[0] != 1 or b[236:240] != DHCP_MAGIC:
            return True
        xid = b[4:8]
        chaddr = b[28:34]
        kind = None
        opts = b[240:]
        i = 0
        while i < len(opts) and opts[i] != 255:
            if opts[i] == 0:
                i += 1
                continue
            if i + 1 >= len(opts):
                break
            code, n = opts[i], opts[i + 1]
            if code == 53 and n == 1 and i + 2 < len(opts):
                kind = opts[i + 2]
            i += 2 + n
        if kind not in (DHCP_DISCOVER, DHCP_REQUEST) or chaddr != bytes.fromhex(self.mac(src).replace(":", "")):
            return True
        yi = ip_bytes(self.ip(src))
        reply = bytearray(240)
        reply[0], reply[1], reply[2] = 2, 1, 6
        reply[4:8] = xid
        reply[16:20] = yi
        reply[20:24] = ip_bytes(HUB_IP)
        reply[28:34] = chaddr
        reply[236:240] = DHCP_MAGIC
        answer = DHCP_OFFER if kind == DHCP_DISCOVER else DHCP_ACK
        reply += bytes([53, 1, answer]) + bytes([54, 4]) + ip_bytes(HUB_IP)
        reply += bytes([51, 4]) + struct.pack("!I", LEASE_SECS) + bytes([1, 4]) + ip_bytes(MASK)
        reply += bytes([3, 4]) + ip_bytes(HUB_IP) + bytes([255])
        src_ip, dst_ip = ip_bytes(HUB_IP), b"\xff\xff\xff\xff"
        packet = ipv4(src_ip, dst_ip, UDP, udp(src_ip, dst_ip, DHCP_SERVER_PORT, DHCP_CLIENT_PORT, bytes(reply)))
        self.send_to(src, chaddr + HUB_MAC + struct.pack("!H", ETH_IP) + packet)
        self.dhcp_answers += 1
        if answer == DHCP_ACK:
            self.acked.add(src)
        return True

    def arp(self, src, frame):
        """ARP for one of the hub's addresses, answered: True when it was
        that."""
        if len(frame) < 42 or struct.unpack("!H", frame[12:14])[0] != ETH_ARP:
            return False
        op = struct.unpack("!H", frame[20:22])[0]
        tpa = frame[38:42]
        if op != 1 or tpa not in (ip_bytes(HUB_IP), ip_bytes(SHELL_IP)):
            return False
        sha, spa = frame[22:28], frame[28:32]
        body = struct.pack("!HHBBH", 1, ETH_IP, 6, 4, 2) + HUB_MAC + tpa + sha + spa
        self.send_to(src, sha + HUB_MAC + struct.pack("!H", ETH_ARP) + body)
        return True

    def from_shell(self, i, data, peer):
        """A datagram of the test's for port i's VM's UDP shell: sent to it
        from the hub's shell address."""
        self.shell_peer[i] = peer
        src, dst = ip_bytes(SHELL_IP), ip_bytes(self.ip(i))
        packet = ipv4(src, dst, UDP, udp(src, dst, SHELL_SPORT + i, self.shell_port, data))
        self.send_to(i, bytes.fromhex(self.mac(i).replace(":", "")) + HUB_MAC + struct.pack("!H", ETH_IP) + packet)

    def to_shell(self, src, frame):
        """A VM's answer to the test's shell, handed back: True when it was
        one."""
        if len(frame) < 14 + 20 + 8 or struct.unpack("!H", frame[12:14])[0] != ETH_IP or frame[23] != UDP:
            return False
        if frame[30:34] != ip_bytes(SHELL_IP):
            return False
        ihl = (frame[14] & 0x0F) * 4
        u = 14 + ihl
        dport = struct.unpack("!H", frame[u + 2:u + 4])[0]
        length = struct.unpack("!H", frame[u + 4:u + 6])[0]
        peer = self.shell_peer.get(src)
        if dport == SHELL_SPORT + src and peer is not None:
            try:
                self.shells[src].sendto(frame[u + 8:u + length], peer)
            except OSError:
                pass
        return True

    def serve(self):
        socks = {p.sock: p for p in self.ports}
        shells = {s: i for i, s in enumerate(self.shells)}
        while not self.stop:
            now = time.monotonic()
            while self.later and self.later[0][0] <= now:
                _, _, dst, frame = heapq.heappop(self.later)
                self.send_to(dst, frame)
            wait = min(0.05, max(0.0, self.later[0][0] - now)) if self.later else 0.05
            ready, _, _ = select.select(list(socks) + list(shells), [], [], wait)
            now = time.monotonic()
            for s in ready:
                if s in shells:
                    try:
                        data, peer = s.recvfrom(65536)
                    except (BlockingIOError, OSError):
                        continue
                    self.from_shell(shells[s], data, peer)
                    continue
                p = socks[s]
                while True:
                    try:
                        frame = s.recv(65536)
                    except (BlockingIOError, OSError):
                        break
                    p.frames_in += 1
                    n = self.tcp_data(frame)
                    if n is not None:
                        p.segments["ack" if n == 0 else "tiny" if n < 100 else "mid" if n < 1400 else "full"] += 1
                    if (len(frame) < 14 or self.dhcp(p.index, frame) or self.arp(p.index, frame)
                            or self.to_shell(p.index, frame)):
                        continue
                    dst_mac, src_mac = frame[0:6], frame[6:12]
                    self.macs[src_mac] = p.index
                    dst = self.macs.get(dst_mac)
                    if dst_mac == BROADCAST or dst_mac[0] & 1 or dst is None:
                        for q in self.ports:
                            if q.index != p.index:
                                self.forward(p.index, q.index, frame, now)
                    elif dst != p.index:
                        self.forward(p.index, dst, frame, now)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--ports", type=int, default=2)
    ap.add_argument("--base", type=int, default=47000)
    args = ap.parse_args()
    hub = Hub(args.ports, args.base).start()
    for i in range(args.ports):
        print("port %d: %s  mac=%s  ip=%s" % (i, hub.netdev(i), hub.mac(i), hub.ip(i)), flush=True)
    try:
        while True:
            time.sleep(1)
    except KeyboardInterrupt:
        hub.close()


if __name__ == "__main__":
    main()
