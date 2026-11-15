#!/usr/bin/env python3
"""DHCP for the demo stand's nos VMs on the AX41's bridge nosbr0 (it began as
N11's, plans/nbs.md): each VM's MAC its fixed address, the bridge's address
the router; any other MAC unanswered. Binds UDP 67 on nosbr0 alone
(SO_BINDTODEVICE), so it answers nothing elsewhere; run as root, detached, a
line logged an answer."""
import socket
import struct
import time

BRIDGE = b"nosbr0"
SERVER = "10.77.0.1"
MASK = "255.255.255.0"
DNS = "185.12.64.1"
LEASE_SECS = 7 * 86400
HOSTS = {
    "52:54:00:77:00:11": "10.77.0.11",
    "52:54:00:77:00:12": "10.77.0.12",
    "52:54:00:77:00:13": "10.77.0.13",
}
MAGIC = b"\x63\x82\x53\x63"
DISCOVER, OFFER, REQUEST, ACK = 1, 2, 3, 5


def ipb(text):
    return socket.inet_aton(text)


def kind_of(opts):
    i = 0
    while i < len(opts) and opts[i] != 255:
        if opts[i] == 0:
            i += 1
            continue
        if i + 1 >= len(opts):
            return None
        code, n = opts[i], opts[i + 1]
        if code == 53 and n == 1 and i + 2 < len(opts):
            return opts[i + 2]
        i += 2 + n
    return None


def main():
    macs = {bytes.fromhex(m.replace(":", "")): ip for m, ip in HOSTS.items()}
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_BINDTODEVICE, BRIDGE)
    s.bind(("0.0.0.0", 67))
    print(time.strftime("%H:%M:%S"), "serving on", BRIDGE.decode(), flush=True)
    while True:
        b, _ = s.recvfrom(4096)
        if len(b) < 240 or b[0] != 1 or b[236:240] != MAGIC:
            continue
        chaddr = b[28:34]
        kind = kind_of(b[240:])
        ip = macs.get(chaddr)
        if kind not in (DISCOVER, REQUEST) or ip is None:
            continue
        r = bytearray(240)
        r[0], r[1], r[2] = 2, 1, 6
        r[4:8] = b[4:8]
        r[10:12] = b[10:12]
        r[16:20] = ipb(ip)
        r[20:24] = ipb(SERVER)
        r[28:34] = chaddr
        r[236:240] = MAGIC
        answer = OFFER if kind == DISCOVER else ACK
        r += bytes([53, 1, answer, 54, 4]) + ipb(SERVER)
        r += bytes([51, 4]) + struct.pack("!I", LEASE_SECS)
        r += bytes([1, 4]) + ipb(MASK) + bytes([3, 4]) + ipb(SERVER) + bytes([6, 4]) + ipb(DNS)
        r += bytes([255])
        s.sendto(bytes(r), ("255.255.255.255", 68))
        print(time.strftime("%H:%M:%S"), "offer" if answer == OFFER else "ack", chaddr.hex(":"), ip, flush=True)


if __name__ == "__main__":
    main()
