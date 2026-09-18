#!/usr/bin/env python3
"""Partition table test: boot with an MBR disk and a GPT disk attached, and
check what the kernel made of them.

The tables are written here, by hand, so the test needs no partitioning tool
and no privileges -- and so the bytes the kernel reads are exactly the bytes
this file says.

    scripts/parttest.py                 # arm64 (HVF where there is one)
    scripts/parttest.py --arch x86_64   # x86, over the ISO
    scripts/parttest.py --tcg           # arm64 without HVF

arm64 checks the shell's view over the UDP shell as well as the kernel log;
x86 boots the ISO, whose command line carries no UDP shell, so it checks the
log alone. Exit code 0 = every check passed.
"""

import argparse
import os
import platform
import signal
import socket
import re
import struct
import subprocess
import sys
import tempfile
import time
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

SECTOR = 512
# The MBR disk holds a partition big enough for a nanofs (17409 blocks of
# 4 KiB, fs/nanofs.h), so a filesystem can be mounted on it; both images are
# sparse, so their size costs nothing.
MBR_DISK_MB = 128
GPT_DISK_MB = 64
SHELL_PORT = 9100

# The UDP shell's framing (scripts/udpsh.py)
SHELL_MAGIC = 0x4E4F5348
SHELL_HDR = struct.Struct("!IIHHHH")
SHELL_LAST = 1

# The MBR disk: a partition to put a filesystem on, and a smaller one kept
# raw, marked at its first sector
MBR_PARTS = ((2048, 147456, 0x83), (155648, 8192, 0x83))
# The GPT disk: first and last LBA, last included
GPT_PARTS = ((2048, 10239), (10240, 14335))

# "Linux filesystem data" (0FC63DAF-8483-4772-8E79-3D69D8477DE4), stored the
# way a GPT stores it: the first three fields little-endian.
# Data1/Data2/Data3 are stored little-endian, the rest as printed.
LINUX_DATA_GUID = bytes.fromhex("af3dc60f" + "8384" + "7247" + "8e793d69d8477de4")
DISK_GUID = bytes(range(16))

failures = []


def check(name, ok, detail=""):
    print(("PASS: " if ok else "FAIL: ") + name + ("\n      " + str(detail) if detail and not ok else ""),
          flush=True)
    if not ok:
        failures.append(name)
    return ok


def marker(index):
    """What the first sector of a partition is filled with, so a read of the
    partition can be told from a read of the disk around it."""
    return (("NOS PARTITION %d " % index).encode() * 32)[:SECTOR]


def sparse(path, size):
    with open(path, "wb") as f:
        f.truncate(size)


def put(path, lba, data):
    with open(path, "r+b") as f:
        f.seek(lba * SECTOR)
        f.write(data)


def mbr_image(path):
    """A disk with a plain MBR: two partitions, no GPT anywhere."""
    sparse(path, MBR_DISK_MB * 1024 * 1024)

    sector = bytearray(SECTOR)
    for i, (start, count, ptype) in enumerate(MBR_PARTS):
        at = 446 + i * 16
        sector[at] = 0x00                            # not bootable
        sector[at + 1:at + 4] = b"\xfe\xff\xff"      # CHS, which nothing reads
        sector[at + 4] = ptype
        sector[at + 5:at + 8] = b"\xfe\xff\xff"
        sector[at + 8:at + 12] = struct.pack("<I", start)
        sector[at + 12:at + 16] = struct.pack("<I", count)
    sector[510:512] = b"\x55\xaa"
    put(path, 0, bytes(sector))

    for i, (start, _count, _type) in enumerate(MBR_PARTS):
        put(path, start, marker(i + 1))


def gpt_image(path):
    """A GPT disk: the protective MBR that says to look at LBA 1, a header
    with the checksum the kernel verifies, and an entry array."""
    total = GPT_DISK_MB * 1024 * 1024 // SECTOR
    sparse(path, GPT_DISK_MB * 1024 * 1024)

    # The protective MBR: one entry of type 0xEE over the whole disk
    sector = bytearray(SECTOR)
    at = 446
    sector[at + 1:at + 4] = b"\x00\x02\x00"
    sector[at + 4] = 0xEE
    sector[at + 5:at + 8] = b"\xff\xff\xff"
    sector[at + 8:at + 12] = struct.pack("<I", 1)
    sector[at + 12:at + 16] = struct.pack("<I", min(total - 1, 0xFFFFFFFF))
    sector[510:512] = b"\x55\xaa"
    put(path, 0, bytes(sector))

    entries = bytearray(128 * 128)
    for i, (first, last) in enumerate(GPT_PARTS):
        at = i * 128
        entries[at:at + 16] = LINUX_DATA_GUID
        entries[at + 16:at + 32] = bytes([i + 1] * 16)
        entries[at + 32:at + 40] = struct.pack("<Q", first)
        entries[at + 40:at + 48] = struct.pack("<Q", last)
    put(path, 2, bytes(entries))

    header = bytearray(92)
    header[0:8] = b"EFI PART"
    header[8:12] = struct.pack("<I", 0x00010000)     # revision 1.0
    header[12:16] = struct.pack("<I", 92)
    header[16:20] = b"\x00\x00\x00\x00"              # the checksum, taken last
    header[24:32] = struct.pack("<Q", 1)             # this header
    header[32:40] = struct.pack("<Q", total - 1)     # the backup, not written
    header[40:48] = struct.pack("<Q", 34)
    header[48:56] = struct.pack("<Q", total - 34)
    header[56:72] = DISK_GUID
    header[72:80] = struct.pack("<Q", 2)             # the entry array
    header[80:84] = struct.pack("<I", 128)           # slots
    header[84:88] = struct.pack("<I", 128)           # bytes each
    header[88:92] = struct.pack("<I", zlib.crc32(bytes(entries)) & 0xFFFFFFFF)
    header[16:20] = struct.pack("<I", zlib.crc32(bytes(header)) & 0xFFFFFFFF)
    put(path, 1, bytes(header))

    for i, (first, _last) in enumerate(GPT_PARTS):
        put(path, first, marker(i + 1))


class Shell:
    """The UDP shell, answering whole (the framing of scripts/udpsh.py)."""

    def __init__(self, timeout=60):
        self.seq = 0
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.settimeout(timeout)

    def run(self, cmd):
        self.seq += 1
        body = cmd.encode()
        self.sock.sendto(SHELL_HDR.pack(SHELL_MAGIC, self.seq, 0, 0, len(body), 0) + body,
                         ("127.0.0.1", SHELL_PORT))
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
                out = b"".join(chunks[i] for i in range(last + 1)).decode(errors="replace")
                print("  $ " + cmd + "".join("\n    " + line for line in out.splitlines()), flush=True)
                return out


def wait_log(log, marker_text, timeout):
    start = time.time()
    while time.time() - start < timeout:
        text = open(log, errors="replace").read() if os.path.exists(log) else ""
        if marker_text in text:
            return True
        if "PANIC" in text:
            print(text[-3000:])
            sys.exit("kernel panic")
        time.sleep(0.5)
    return False


def kill(p):
    if p is not None and p.poll() is None:
        p.send_signal(signal.SIGTERM)
        try:
            p.wait(10)
        except subprocess.TimeoutExpired:
            p.kill()


def dump_bytes(out):
    """The byte columns of a `diskread` hex dump, without the addresses --
    what makes two reads of the same data comparable."""
    rows = []
    for line in out.splitlines():
        if ":" in line:
            rows.append(line.split(":", 1)[1].split())
    return rows


def mkrootfs(image):
    subprocess.run([os.path.join(HERE, "mkrootfs.sh"), image, "64", "", "1024"],
                   cwd=ROOT, check=True, stdout=subprocess.DEVNULL)


def disks_of(tmp):
    root = os.path.join(tmp, "root.img")
    mbr = os.path.join(tmp, "mbr.img")
    gpt = os.path.join(tmp, "gpt.img")
    mkrootfs(root)
    mbr_image(mbr)
    gpt_image(gpt)
    return root, mbr, gpt


PART_LINE = re.compile(r"part: (\w+) start (\d+) size (\d+)")


def expected_parts():
    """Every (start, sectors) the two tables written above ask for. The four
    are distinct, so a partition can be recognised by its pair alone -- which
    is what keeps this test from caring which disk QEMU hands the kernel
    first (arm64 gives the virtio-mmio slots out in reverse)."""
    mbr = [(start, count) for start, count, _type in MBR_PARTS]
    gpt = [(first, last - first + 1) for first, last in GPT_PARTS]
    return mbr, gpt


def log_checks(log):
    """What the probe traced, on either architecture."""
    text = open(log, errors="replace").read()
    traced = " | ".join(l.split(": ", 1)[-1] for l in text.splitlines() if "part:" in l)

    parts = {}
    for line in text.splitlines():
        found = PART_LINE.search(line)
        if found:
            parts[found.group(1)] = (int(found.group(2)), int(found.group(3)))

    mbr, gpt = expected_parts()
    check("log: the four partitions written, and only those",
          sorted(parts.values()) == sorted(mbr + gpt), traced)

    for name, pair in (("mbr", mbr), ("gpt", gpt)):
        on = [n for n, p in parts.items() if p in pair]
        check("log: the %s pair is on one disk, numbered 1 and 2" % name,
              len(on) == 2 and sorted(n[-1] for n in on) == ["1", "2"]
              and on[0][:-1] == on[1][:-1], traced)

    check("log: nothing on the table-less root disk", len(parts) == 4, traced)
    check("log: three disks looked at", "on 3 disks" in text, traced)
    return parts


def shell_checks(sh, parts):
    mbr, gpt = expected_parts()
    listing = sh.run("disks")

    rows = {}
    for line in listing.splitlines():
        fields = line.split()
        if len(fields) >= 2 and fields[1].isdigit():
            rows[fields[0]] = int(fields[1])

    for name, (_start, count) in parts.items():
        check("disks lists %s with %d sectors" % (name, count),
              rows.get(name) == count, listing)

    """Which disk carries which table, asked of the kernel rather than
    assumed: the disks are the names in `disks` that are not partitions."""
    disks = [n for n in rows if n not in parts]
    tables = {}
    for disk in sorted(disks):
        out = sh.run("partitions " + disk)
        tables[disk] = out
        kind = out.split()[0].rstrip(",") if out.split() else ""
        check("partitions %s answers" % disk, kind in ("mbr", "gpt", "no"), out)

    mbr_disk = [d for d, out in tables.items() if out.lstrip().startswith("mbr")]
    gpt_disk = [d for d, out in tables.items() if out.lstrip().startswith("gpt")]
    bare = [d for d, out in tables.items() if out.lstrip().startswith("no partition table")]
    check("one disk reads as mbr", len(mbr_disk) == 1, tables)
    check("one disk reads as gpt", len(gpt_disk) == 1, tables)
    check("the root disk reads as untabled", len(bare) == 1, tables)
    if not (mbr_disk and gpt_disk):
        return

    out = tables[mbr_disk[0]]
    check("the mbr table has both entries and their type",
          all(str(start) in out for start, _c in mbr) and "0x83" in out, out)

    out = tables[gpt_disk[0]]
    check("the gpt header checksum was accepted", "MISMATCH" not in out, out)
    check("the gpt table names the type guid",
          "0FC63DAF-8483-4772-8E79-3D69D8477DE4" in out, out)
    check("the gpt table has both entries",
          all(str(start) in out for start, _c in gpt), out)

    # The offset: a partition's sector 0 is its first sector on the disk
    disk = mbr_disk[0]
    raw = disk + "2"
    start, count = parts[raw]
    inside = dump_bytes(sh.run("diskread %s 0" % raw))
    behind = dump_bytes(sh.run("diskread %s %d" % (disk, start)))
    check("%s sector 0 is %s sector %d" % (raw, disk, start),
          bool(inside) and inside == behind)
    check("%s sector 0 carries the marker" % raw,
          bool(inside) and bytes(int(b, 16) for b in inside[0]).startswith(b"NOS PARTITION 2"))

    # And the end: a read past the partition is refused, not served from the
    # disk behind it
    past = sh.run("diskread %s %d" % (raw, count))
    check("a read past %s is refused" % raw, "read error" in past, past)

    # The claim: what the probe registers is linked to the disk it is on, so
    # a filesystem mounted on the partition keeps writers off both.
    fs = disk + "1"
    sh.run("mkdir /mnt")
    formatted = sh.run("format nanofs " + fs)
    check("formatting %s" % fs, "error" not in formatted.lower(), formatted)
    mounted = sh.run("mount nanofs %s /mnt" % fs)
    check("mounting %s" % fs, "mounted" in mounted, mounted)

    refused = sh.run("diskwrite %s %d 55" % (disk, parts[fs][0]))
    check("a write to the disk under a mounted partition is refused",
          "in use by" in refused, refused)
    refused = sh.run("diskwrite %s 0 55" % fs)
    check("a write to the mounted partition itself is refused",
          "in use by" in refused, refused)

    sh.run("umount /mnt")
    allowed = sh.run("diskwrite %s %d 55" % (disk, parts[fs][0]))
    check("and it is allowed again once unmounted", "in use by" not in allowed, allowed)


def arm64(tmp, tcg):
    root, mbr, gpt = disks_of(tmp)
    log = os.path.join(tmp, "serial.log")

    hvf = platform.system() == "Darwin" and platform.machine() == "arm64" and not tcg
    accel = ["-accel", "hvf", "-cpu", "host"] if hvf else ["-accel", "tcg", "-cpu", "cortex-a72"]

    argv = ["qemu-system-aarch64", "-M", "virt,gic-version=3", "-smp", "4", "-m", "1024"] + accel + [
        "-kernel", os.path.join(ROOT, "nos-arm64.img"),
        "-append", "root=auto udpshell=%d" % SHELL_PORT,
        "-global", "virtio-mmio.force-legacy=false",
        "-drive", "file=%s,format=raw,id=hd0,if=none" % root, "-device", "virtio-blk-device,drive=hd0",
        "-drive", "file=%s,format=raw,id=hd1,if=none" % mbr, "-device", "virtio-blk-device,drive=hd1",
        "-drive", "file=%s,format=raw,id=hd2,if=none" % gpt, "-device", "virtio-blk-device,drive=hd2",
        "-device", "virtio-net-device,netdev=net0",
        "-netdev", "user,id=net0,hostfwd=udp::%d-:%d" % (SHELL_PORT, SHELL_PORT),
        "-serial", "file:" + log, "-display", "none"]

    p = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        if not check("boots", wait_log(log, "boot: complete", 120 if hvf else 300)):
            return
        parts = log_checks(log)
        shell_checks(Shell(), parts)
    finally:
        kill(p)


def x86(tmp):
    root, mbr, gpt = disks_of(tmp)
    log = os.path.join(tmp, "serial.log")

    argv = ["qemu-system-x86_64", "-display", "none", "-m", "1G", "-smp", "2",
            "-cdrom", os.path.join(ROOT, "nos.iso"),
            "-drive", "file=%s,format=raw,id=hd0,if=none" % root,
            "-device", "virtio-blk-pci,drive=hd0,disable-legacy=on,disable-modern=off",
            "-drive", "file=%s,format=raw,id=hd1,if=none" % mbr,
            "-device", "virtio-blk-pci,drive=hd1,disable-legacy=on,disable-modern=off",
            "-drive", "file=%s,format=raw,id=hd2,if=none" % gpt,
            "-device", "virtio-blk-pci,drive=hd2,disable-legacy=on,disable-modern=off",
            "-serial", "file:" + log]

    p = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        if not check("boots", wait_log(log, "boot: complete", 300)):
            return
        log_checks(log)
    finally:
        kill(p)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--arch", choices=("aarch64", "x86_64"), default="aarch64")
    ap.add_argument("--tcg", action="store_true", help="arm64: do not use HVF")
    args = ap.parse_args()

    with tempfile.TemporaryDirectory(prefix="nos-parttest-") as tmp:
        if args.arch == "aarch64":
            arm64(tmp, args.tcg)
        else:
            x86(tmp)

    print()
    if failures:
        print("FAILED: " + ", ".join(failures))
        return 1
    print("parttest: OK (%s)" % args.arch)
    return 0


if __name__ == "__main__":
    sys.exit(main())
