#!/usr/bin/env python3
"""UDP shell client for nos kernel."""

import socket
import struct
import sys

MAGIC = 0x4E4F5348  # "NOSH"
HDR_FMT = '!IIHHHH'  # Magic, SeqNo, ChunkIdx, Flags, PayloadLen, Reserved
HDR_SIZE = struct.calcsize(HDR_FMT)  # 16
FLAG_LAST = 0x0001

class UdpShellClient:
    def __init__(self, host, port=9000, timeout=30.0):
        self.host = host
        self.port = port
        self.timeout = timeout
        self.sock = None
        self.seq = 0

    def connect(self):
        """Create a fresh UDP socket and reset sequence."""
        self.close()
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.settimeout(self.timeout)
        self.seq = 0

    def close(self):
        """Close the socket if open."""
        if self.sock:
            self.sock.close()
            self.sock = None

    def send(self, cmd):
        """Send a command string. Returns True on success."""
        cmd_bytes = cmd.encode()
        hdr = struct.pack(HDR_FMT, MAGIC, self.seq, 0, 0, len(cmd_bytes), 0)
        self.sock.sendto(hdr + cmd_bytes, (self.host, self.port))
        return True

    def recv(self):
        """Receive all reply chunks. Returns True on success, False on error."""
        expect_idx = 0

        while True:
            try:
                data, _ = self.sock.recvfrom(4096)
            except socket.timeout:
                print("[timeout, reconnecting]", file=sys.stderr)
                return False

            if len(data) < HDR_SIZE:
                continue

            magic, seq_no, chunk_idx, flags, payload_len, _ = struct.unpack(HDR_FMT, data[:HDR_SIZE])

            if magic != MAGIC:
                continue
            if seq_no != self.seq:
                continue
            if payload_len > len(data) - HDR_SIZE:
                print("\n[protocol error: payload_len mismatch, reconnecting]", file=sys.stderr)
                return False
            if chunk_idx != expect_idx:
                print(f"\n[protocol error: expected chunk {expect_idx}, got {chunk_idx}, reconnecting]", file=sys.stderr)
                return False

            if payload_len > 0:
                print(data[HDR_SIZE:HDR_SIZE + payload_len].decode(errors="replace"), end="")

            expect_idx += 1

            if flags & FLAG_LAST:
                return True

    def execute(self, cmd):
        """Send command and receive reply. Reconnects on error."""
        self.send(cmd)
        if self.recv():
            self.seq += 1
        else:
            self.connect()

    def run(self):
        """Interactive shell loop."""
        self.connect()
        print(f"nos udp shell -> {self.host}:{self.port}")

        while True:
            try:
                cmd = input("$ ")
            except (EOFError, KeyboardInterrupt):
                print()
                break

            if not cmd.strip():
                continue

            self.execute(cmd)

        self.close()

def main():
    if len(sys.argv) < 2:
        print("usage: udpsh.py <host> [port] [timeout]")
        sys.exit(1)

    host = sys.argv[1]
    port = int(sys.argv[2]) if len(sys.argv) > 2 else 9000
    timeout = float(sys.argv[3]) if len(sys.argv) > 3 else 30.0

    client = UdpShellClient(host, port, timeout)
    client.run()

if __name__ == "__main__":
    main()
