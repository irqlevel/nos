#!/usr/bin/env python3
"""Prepare and read the raw disk area the kernel writes its log to.

The kernel writes its log, line by line and synchronously, to a raw area on a
block device -- for the machine that has no serial port, no working NIC and so
no netconsole, and stops somewhere in boot without saying anything. See
src/cpp/kernel/disklog.h.

The area is never guessed by the kernel. It writes only where it finds the
header this tool lays down, magic and checksum intact, in the first sector of
a block device. A disk that has not been through `format` here is not written
to at all.

    sudo ./scripts/disklog.py format /dev/nvme0n1p1      # prepare an area
    sudo ./scripts/disklog.py read   /dev/nvme0n1p1      # read the last boot

`format` refuses a device that still looks like it holds something -- a
filesystem, a RAID member, a swap area -- unless told otherwise. That check is
the only thing standing between a typo and somebody's root filesystem, so it
is on by default and says exactly what it found.
"""

import argparse
import struct
import sys
import zlib

MAGIC = 0x0031474F4C534F4E  # "NOSLOG1\0" read back as a little-endian u64
VERSION = 1
HEADER_FMT = "<QIIQQQII"
HEADER_SIZE = struct.calcsize(HEADER_FMT)   # 48
CRC_COVERS = 40                              # everything before the Crc field

assert HEADER_SIZE == 48, HEADER_SIZE

# Default area: 64 MiB, which is a great many boots' worth of log.
DEFAULT_BYTES = 64 * 1024 * 1024


def pack_header(sector_size, area_sectors, boot_seq, log_bytes):
    body = struct.pack("<QIIQQQ", MAGIC, VERSION, sector_size,
                       area_sectors, boot_seq, log_bytes)
    assert len(body) == CRC_COVERS, len(body)
    crc = zlib.crc32(body) & 0xFFFFFFFF
    return body + struct.pack("<II", crc, 0)


def parse_header(buf):
    if len(buf) < HEADER_SIZE:
        return None
    magic, version, sector_size, area_sectors, boot_seq, log_bytes, crc, _ = \
        struct.unpack(HEADER_FMT, buf[:HEADER_SIZE])
    if magic != MAGIC or version != VERSION:
        return None
    if (zlib.crc32(buf[:CRC_COVERS]) & 0xFFFFFFFF) != crc:
        return None
    return {
        "sector_size": sector_size,
        "area_sectors": area_sectors,
        "boot_seq": boot_seq,
        "log_bytes": log_bytes,
    }


# Signatures worth refusing to overwrite. Offset, expected bytes, what it is.
SIGNATURES = [
    (0x438, b"\x53\xef", "ext2/3/4 filesystem"),
    (0x1000, b"\xfc\x4e\x2b\xa9", "mdadm RAID member (1.1/1.2 metadata)"),
    (0x1000, b"LUKS\xba\xbe", "LUKS container"),
    (0x10040, b"\x0b\xf5\x03\x00", "btrfs filesystem"),
    (0xFF6, b"SWAPSPACE2", "swap area"),
    (0xFF6, b"SWAP-SPACE", "swap area"),
    (0x200, b"EFI PART", "GPT partition table"),
    (0x36, b"FAT", "FAT filesystem"),
    (0x52, b"FAT32", "FAT32 filesystem"),
    (0x3, b"NTFS    ", "NTFS filesystem"),
    (0x10000, b"XFSB", "XFS filesystem"),
]


def sniff(path):
    """What the start of the device looks like it already holds."""
    found = []
    with open(path, "rb") as f:
        head = f.read(0x11000)
    for off, want, what in SIGNATURES:
        if head[off:off + len(want)] == want and what not in found:
            found.append(what)
    return found


def sector_size_of(path):
    """Logical sector size, from sysfs where it is available."""
    import os
    name = os.path.basename(os.path.realpath(path))
    for cand in ("/sys/block/%s/queue/logical_block_size" % name,
                 "/sys/class/block/%s/queue/logical_block_size" % name):
        try:
            with open(cand) as f:
                return int(f.read().strip())
        except OSError:
            continue
    # Partitions inherit the parent's; fall back to the usual value.
    return 512


def device_bytes(path):
    with open(path, "rb") as f:
        f.seek(0, 2)
        return f.tell()


def cmd_format(args):
    existing = sniff(args.device)
    if existing and not args.force:
        print("refusing to format %s: it looks like it holds %s"
              % (args.device, ", ".join(existing)), file=sys.stderr)
        print("pass --force only if you are certain that is stale.",
              file=sys.stderr)
        return 1

    sector_size = args.sector_size or sector_size_of(args.device)
    total = device_bytes(args.device)
    if total == 0:
        print("%s has no size" % args.device, file=sys.stderr)
        return 1

    want = min(args.bytes, total)
    area_sectors = want // sector_size
    if area_sectors < 2:
        print("%s is too small for a log area" % args.device, file=sys.stderr)
        return 1

    header = pack_header(sector_size, area_sectors, 0, 0)
    header += b"\0" * (sector_size - len(header))

    with open(args.device, "r+b") as f:
        f.write(header)
        # Clear the first megabyte of text so a short first boot is not read
        # against whatever was on the disk before.
        f.write(b"\0" * min(1024 * 1024, (area_sectors - 1) * sector_size))
        f.flush()
        import os
        os.fsync(f.fileno())

    print("prepared %s: %d sectors of %d bytes (%.1f MiB), boot seq 0"
          % (args.device, area_sectors, sector_size,
             area_sectors * sector_size / (1024 * 1024)))
    if existing:
        print("(overwrote what looked like %s)" % ", ".join(existing))
    return 0


def cmd_read(args):
    sector_size = args.sector_size or sector_size_of(args.device)
    with open(args.device, "rb") as f:
        head = f.read(sector_size)
        hdr = parse_header(head)
        if hdr is None:
            print("%s carries no log header (never formatted, or overwritten)"
                  % args.device, file=sys.stderr)
            return 1

        text_sectors = hdr["area_sectors"] - 1

        # Do not trust the recorded length to be the end. It is written as
        # whole sectors are retired, and the whole point of this channel is
        # the boot that stopped -- where the last thing written is a partial
        # sector the header never caught up with, and where the header may
        # not have been updated at all. Read generously and let the zeroes the
        # area was formatted with end the text.
        want = min(max(hdr["log_bytes"] * 2, args.max_bytes),
                   text_sectors * sector_size)
        data = f.read(want)

    end = data.find(b"\0")
    if end >= 0:
        data = data[:end]

    sys.stderr.write("boot seq %d, %d bytes recorded, %d read\n"
                     % (hdr["boot_seq"], hdr["log_bytes"], len(data)))

    out = open(args.out, "wb") if args.out else sys.stdout.buffer
    out.write(data)
    if args.out:
        out.close()
        print("written to %s" % args.out, file=sys.stderr)
    return 0


def main():
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)

    f = sub.add_parser("format", help="lay down a log area header")
    f.add_argument("device")
    f.add_argument("-b", "--bytes", type=int, default=DEFAULT_BYTES,
                   help="area size in bytes (default %d)" % DEFAULT_BYTES)
    f.add_argument("--sector-size", type=int, default=0)
    f.add_argument("--force", action="store_true",
                   help="format even though the device looks occupied")
    f.set_defaults(func=cmd_format)

    r = sub.add_parser("read", help="print the recorded log")
    r.add_argument("device")
    r.add_argument("-o", "--out", help="write to a file instead of stdout")
    r.add_argument("--sector-size", type=int, default=0)
    r.add_argument("--max-bytes", type=int, default=32 * 1024 * 1024,
                   help="how much of the area to scan (default 32 MiB)")
    r.set_defaults(func=cmd_read)

    args = p.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
