#!/usr/bin/env python3
"""End-to-end test of the netblk module (docs/netblk.md), in QEMU.

Boots the kernel with an NVMe disk and a virtio-net card, loads netblk.ko off
the root filesystem over the UDP shell, serves the NVMe disk on a UDP port,
and works it from outside with scripts/netblk.py: its geometry, data written
and read back, requests it has to refuse, a short load test each way, what
the kernel's own reads find on the disk afterwards, and a stop and an rmmod
that leave nothing behind. Fails on any mismatch and on a panic.

    scripts/netblk-test.py [--arch x86_64|aarch64] [--tcg] [--keep]

x86-64 boots bin/kernel64.elf from an ISO, under KVM unless --tcg; arm64
boots nos-arm64.img on QEMU's virt board under TCG, its disks and network on
virtio-mmio and the NVMe disk on PCIe. After `make` (and `make ARCH=aarch64`),
inside nos-builder, which has both QEMUs:

    docker run --rm --device /dev/kvm -v /root/nos:/root/nos -w /root/nos \\
        nos-builder scripts/netblk-test.py
"""

import argparse
import os
import random
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
sys.path.insert(0, HERE)

import netblk  # noqa: E402  (scripts/netblk.py)

KERNELS = {
    "x86_64": os.path.join(ROOT, "bin", "kernel64.elf"),
    "aarch64": os.path.join(ROOT, "nos-arm64.img"),
}

CMDLINE = "dhcp=auto dns=on root=auto udpshell=%d"

SHELL_PORT = 9000
BLK_PORT = 7000
DISK_MIB = 256
SECTOR = 512

# The second disk's one partition, and the port it is served on.
PART_START = 2048
PART_SECTORS = (64 << 20) // SECTOR
PART_PORT = 7002

# The UDP shell's framing (scripts/udpsh.py)
SHELL_MAGIC = 0x4E4F5348
SHELL_HDR = struct.Struct("!IIHHHH")
SHELL_LAST = 1

GRUB_CFG = """insmod all_video
set timeout=0
set default=0
menuentry "nos" {
\tmultiboot2 /boot/kernel64.elf %s
}
"""


def module_path(arch):
    return os.path.join(ROOT, "out", arch, "modules", "netblk.ko")


class TestFailure(Exception):
    pass


def check(cond, what):
    if not cond:
        raise TestFailure(what)
    print(f"  ok: {what}")


class Shell:
    """The UDP shell, one command at a time, the reply returned as text"""

    def __init__(self, port):
        self.addr = ("127.0.0.1", port)
        self.seq = random.getrandbits(16)

    def run(self, cmd, timeout=30.0):
        self.seq += 1
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.settimeout(timeout)
        try:
            data = cmd.encode()
            sock.sendto(SHELL_HDR.pack(SHELL_MAGIC, self.seq, 0, 0, len(data), 0) + data, self.addr)
            chunks, last = {}, None
            while last is None or len(chunks) <= last:
                reply, _ = sock.recvfrom(4096)
                magic, seq, idx, flags, length, _ = SHELL_HDR.unpack_from(reply)
                if magic != SHELL_MAGIC or seq != self.seq:
                    continue
                chunks[idx] = reply[SHELL_HDR.size:SHELL_HDR.size + length]
                if flags & SHELL_LAST:
                    last = idx
            text = b"".join(chunks[i] for i in range(last + 1)).decode(errors="replace")
        finally:
            sock.close()
        print(f"  $ {cmd}\n" + "".join(f"    {line}\n" for line in text.splitlines()), end="")
        return text


def build_images(tmp, arch):
    if arch == "x86_64":
        iso = os.path.join(tmp, "iso", "boot", "grub")
        os.makedirs(iso)
        with open(os.path.join(iso, "grub.cfg"), "w") as f:
            f.write(GRUB_CFG % (CMDLINE % SHELL_PORT))
        shutil.copy(KERNELS[arch], os.path.join(tmp, "iso", "boot", "kernel64.elf"))
        subprocess.run(["grub-mkrescue", "-o", os.path.join(tmp, "test.iso"), os.path.join(tmp, "iso")],
                       check=True, capture_output=True)

    rootdir = os.path.join(tmp, "rootdir")
    os.makedirs(rootdir)
    shutil.copy(module_path(arch), rootdir)
    subprocess.run([os.path.join(HERE, "mkrootfs.sh"), os.path.join(tmp, "root.img"), "64", rootdir],
                   check=True, capture_output=True)

    with open(os.path.join(tmp, "nvme.img"), "wb") as f:
        f.truncate(DISK_MIB << 20)

    # A second disk with one partition on it: what is served there goes
    # through the device table's rebasing, which a whole disk never touches.
    with open(os.path.join(tmp, "nvme-part.img"), "wb") as f:
        f.truncate(DISK_MIB << 20)
        mbr = bytearray(SECTOR)
        entry = struct.pack("<B3sB3sII", 0, b"\xfe\xff\xff", 0x83, b"\xfe\xff\xff",
                            PART_START, PART_SECTORS)
        mbr[446:446 + len(entry)] = entry
        mbr[510:512] = b"\x55\xaa"
        f.seek(0)
        f.write(mbr)


def boot(tmp, arch, tcg, nic="virtio"):
    log = os.path.join(tmp, "serial.log")
    fwd = (f"hostfwd=udp:127.0.0.1:{SHELL_PORT}-:{SHELL_PORT},"
           f"hostfwd=udp:127.0.0.1:{BLK_PORT}-:{BLK_PORT},"
           f"hostfwd=udp:127.0.0.1:{PART_PORT}-:{PART_PORT}")
    disks = [
        "-drive", f"file={os.path.join(tmp, 'root.img')},format=raw,id=root,if=none",
        "-drive", f"file={os.path.join(tmp, 'nvme.img')},format=raw,id=nvme0,if=none",
        "-device", "nvme,serial=netblk0,drive=nvme0",
        "-drive", f"file={os.path.join(tmp, 'nvme-part.img')},format=raw,id=nvme1,if=none",
        "-device", "nvme,serial=netblk1,drive=nvme1",
        "-netdev", f"user,id=net0,{fwd}",
        "-serial", f"file:{log}", "-display", "none", "-m", "1G", "-smp", "4",
    ]
    if arch == "x86_64":
        # QEMU's igb (8.0 and later) is the 82576 the kernel's Rust igb
        # driver claims -- the same driver that runs the I210 on the AX41
        card = ("igb,netdev=net0" if nic == "igb"
                else "virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off")
        cmd = [
            "qemu-system-x86_64", *([] if tcg else ["-enable-kvm", "-cpu", "host"]),
            "-cdrom", os.path.join(tmp, "test.iso"), "-boot", "d",
            "-device", "virtio-blk-pci,drive=root,disable-legacy=on,disable-modern=off",
            "-device", card,
            "-device", "virtio-rng-pci",
        ]
    else:
        cmd = [
            "qemu-system-aarch64", "-M", "virt,gic-version=3", "-accel", "tcg", "-cpu", "cortex-a72",
            "-kernel", KERNELS[arch], "-append", CMDLINE % SHELL_PORT,
            "-global", "virtio-mmio.force-legacy=false",
            "-device", "virtio-blk-device,drive=root",
            "-device", "virtio-net-device,netdev=net0",
            "-device", "virtio-rng-device",
        ]
    return subprocess.Popen(cmd + disks), log


def wait_for_boot(qemu, log, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        time.sleep(1)
        if qemu.poll() is not None:
            raise TestFailure("qemu exited")
        text = open(log, errors="replace").read() if os.path.exists(log) else ""
        if "PANIC:" in text:
            raise TestFailure("kernel panic during boot")
        if "boot: complete" in text:
            return
    raise TestFailure(f"no 'boot: complete' in {timeout} s")


def expect_refused(client, req, status_text):
    try:
        client.run([req], 1)
    except netblk.NetblkError as e:
        check(status_text in str(e), f"refused: {status_text}")
        return
    raise TestFailure(f"expected '{status_text}', the request went through")


def dump_sector(shell, disk, sector):
    """The kernel's own read of one sector, no netblk in the way."""
    dumped = bytearray()
    for line in shell.run(f"diskread {disk} {sector}").splitlines():
        _, colon, rest = line.partition(":")
        if colon:
            dumped.extend(int(token, 16) for token in rest.split())
    return bytes(dumped)


def find_partition(shell):
    """The device the kernel made of the second disk's one partition: the
    disk served first is nvme0 or nvme1 by probe order, so ask."""
    for line in shell.run("disks").splitlines():
        name = line.split()[0] if line.split() else ""
        if name.startswith("nvme") and len(name) > len("nvme0"):
            return name
    return None


def exercise(shell, tcg):
    print("module")
    check("loaded" in shell.run("insmod /netblk.ko"), "netblk.ko loaded")
    check("serving nvme0" in shell.run(f"netblk start nvme0 {BLK_PORT}"), "nvme0 served")
    check("taken" in shell.run(f"netblk start nvme0 {SHELL_PORT} ro"), "the shell's port refused")
    check("in use" in shell.run(f"netblk start nvme0 {BLK_PORT + 1}"), "a second writer refused")

    print("protocol")
    client = netblk.Client("127.0.0.1", BLK_PORT)
    check(client.size == DISK_MIB << 20, f"size {client.size}")
    check(client.sector_size == SECTOR, f"sector size {client.sector_size}")
    check(client.max_io == 1024, f"{client.max_io} bytes a datagram")
    check(not client.read_only, "read-write")

    size = 8 << 20
    data = random.Random(1).randbytes(size)
    client.write(0, data, 64)
    client.flush()
    check(client.read(0, size, 64) == data, "8 MiB written and read back")

    tail = random.Random(2).randbytes(1 << 20)
    client.write(client.size - len(tail), tail, 32, netblk.FLAG_FUA)
    check(client.read(client.size - len(tail), len(tail), 32) == tail, "the last MiB, with FUA")

    expect_refused(client, netblk.Request(netblk.OP_READ, client.size, SECTOR), "out of range")
    expect_refused(client, netblk.Request(netblk.OP_READ, 100, SECTOR), "bad request")
    expect_refused(client, netblk.Request(netblk.OP_READ, 0, client.max_read + SECTOR), "bad request")
    expect_refused(client, netblk.Request(netblk.OP_WRITE, 0, SECTOR, b"\0" * 100), "bad request")

    print("load")
    for mode in ("randread", "randwrite", "read"):
        args = argparse.Namespace(mode=mode, secs=2 if tcg else 3, size=None, bs=None, seed=3, window=64)
        netblk.cmd_bench(client, args)

    # What was written through the network is on the disk: the kernel's own
    # read of the first sector, no netblk in the way
    start = client.read(0, SECTOR, 1)
    check(dump_sector(shell, "nvme0", 0) == start, "the kernel reads what netblk wrote")

    listing = shell.run("netblk list")
    check("port 7000" in listing and "reads " in listing, "listed")

    # A partition: the same path, a disk away. What is written at the start
    # of the partition has to land at the partition's start on the disk --
    # not at the disk's -- and the end of the partition is the end.
    print("a partition")
    part = find_partition(shell)
    check(part is not None, "the second disk's partition is a device")
    check(f"serving {part}" in shell.run(f"netblk start {part} {PART_PORT}"), f"{part} served")

    pclient = netblk.Client("127.0.0.1", PART_PORT)
    check(pclient.size == PART_SECTORS * SECTOR, f"the partition's size, {pclient.size}")

    head = random.Random(4).randbytes(64 * SECTOR)
    pclient.write(0, head, 16)
    last = random.Random(5).randbytes(SECTOR)
    pclient.write(pclient.size - SECTOR, last, 1, netblk.FLAG_FUA)
    pclient.flush()
    check(pclient.read(0, len(head), 16) == head, "written and read back through the partition")
    check(pclient.read(pclient.size - SECTOR, SECTOR, 1) == last, "and its last sector")
    expect_refused(pclient, netblk.Request(netblk.OP_READ, pclient.size, SECTOR), "out of range")

    disk = part[:-1]
    check(dump_sector(shell, disk, PART_START) == head[:SECTOR],
          "the partition's first sector is where the table says it is")
    check(dump_sector(shell, disk, PART_START + PART_SECTORS - 1) == last,
          "and its last one")
    check(dump_sector(shell, disk, 0)[510:512] == b"\x55\xaa",
          "the partition table in front of it is untouched")
    check("stopped" in shell.run(f"netblk stop {PART_PORT}"), "the partition's server stopped")

    print("teardown")
    check("stopped" in shell.run(f"netblk stop {BLK_PORT}"), "stopped")
    check("nothing served" in shell.run("netblk list"), "nothing left")
    check("unloaded" in shell.run("rmmod netblk"), "netblk.ko unloaded")
    check("netblk" not in shell.run("lsmod"), "gone from lsmod")


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--arch", choices=sorted(KERNELS), default="x86_64")
    p.add_argument("--tcg", action="store_true", help="no KVM (always so for aarch64)")
    p.add_argument("--nic", choices=["virtio", "igb"], default="virtio",
                   help="the network card: igb (x86, QEMU 8.0+) runs the Rust igb driver")
    p.add_argument("--keep", action="store_true", help="keep the scratch directory")
    args = p.parse_args()
    tcg = args.tcg or args.arch != "x86_64"
    if args.nic == "igb" and args.arch != "x86_64":
        sys.exit("--nic igb is for x86_64")

    for path in (KERNELS[args.arch], module_path(args.arch)):
        if not os.path.exists(path):
            sys.exit(f"{path} is missing: make first")

    tmp = tempfile.mkdtemp(prefix="netblk-test.")
    qemu = None
    try:
        build_images(tmp, args.arch)
        qemu, log = boot(tmp, args.arch, tcg, args.nic)
        wait_for_boot(qemu, log, 600 if tcg else 120)
        exercise(Shell(SHELL_PORT), tcg)
        if "PANIC:" in open(log, errors="replace").read():
            raise TestFailure("kernel panic")
        print("netblk-test: PASSED")
    except (TestFailure, netblk.NetblkError, OSError, subprocess.CalledProcessError) as e:
        print(f"netblk-test: FAILED: {e}")
        if qemu is not None and os.path.exists(os.path.join(tmp, "serial.log")):
            print("--- last 40 lines of the serial log ---")
            print("".join(open(os.path.join(tmp, "serial.log"), errors="replace").readlines()[-40:]), end="")
        sys.exit(1)
    finally:
        if qemu is not None:
            qemu.kill()
            qemu.wait()
        if args.keep:
            print(f"scratch: {tmp}")
        else:
            shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
