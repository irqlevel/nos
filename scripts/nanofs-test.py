#!/usr/bin/env python3
"""nanofs test: format a disk, work the filesystem from the shell, take it
down, bring it back and see that everything is still there.

nanofs is the kernel's own small checksummed filesystem, and nothing else in
the test suite touches it -- the smoke boots and ext2-test.py both run on
ext2. This is its gate: the boot-time `fstest` over a freshly formatted disk,
then files written, synced, unmounted and read back from a second mount, so
that what is judged is what reached the disk rather than what a cache still
held. Every read verifies the CRC the write put there, so a byte that did
not survive is a failed read, not a wrong answer.

    scripts/nanofs-test.py [--tcg] [--keep]

arm64 only, because it drives the shell over UDP and that is the boot whose
command line carries one; the driver itself is the same code on either
architecture.

Exit code 0 = every check passed.
"""

import argparse
import importlib.util
import os
import re
import signal
import struct
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

spec = importlib.util.spec_from_file_location("pt", os.path.join(HERE, "parttest.py"))
pt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pt)

# mkfs_nanofs.py builds a superblock and a root inode of its own, which is a
# second, independent reading of the on-disk layout. The kernel mounts a disk
# it wrote, and the reader below reads back a disk the kernel wrote: between
# them the two implementations have to agree on every field, which is the
# job e2fsck does for ext2.
spec = importlib.util.spec_from_file_location("mkfs", os.path.join(HERE, "mkfs_nanofs.py"))
mkfs = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mkfs)

SHELL_PORT = pt.SHELL_PORT

# A nanofs takes 1 + 1024 + 16384 blocks of 4 KiB, a little under 68 MiB.
NANO_DISK_MB = 96

# Past the direct blocks of a 4 KiB-block file and well into the block list
FSTEST_SIZE = "200K"

# A file grown one append at a time, over more than one 4 KiB block
BIG_LINES = 200
BIG_LINE = "line-%04d-padding-padding-padding"
BIG_SIZE = BIG_LINES * len(BIG_LINE % 0)


def blank_image(path, megabytes):
    with open(path, "wb") as f:
        f.truncate(megabytes * 1024 * 1024)


def python_formatted_image(path, megabytes):
    """A nanofs written by mkfs_nanofs.py's own idea of the layout."""
    blank_image(path, megabytes)
    with open(path, "r+b") as f:
        f.seek(0)
        f.write(mkfs.build_superblock())
        f.seek(mkfs.INODE_START * mkfs.BLOCK_SIZE)
        f.write(mkfs.build_root_inode())
        f.seek(mkfs.DATA_START * mkfs.BLOCK_SIZE)
        f.write(bytes(mkfs.BLOCK_SIZE))


# Inode fields, as mkfs_nanofs.py lays them out
IN_TYPE = 0
IN_SIZE = 4
IN_NAME = 8
IN_NAME_LEN = 64
IN_PARENT = 72
IN_CHECKSUM = mkfs.INODE_CHECKSUM_OFFSET
IN_DATA_CHECKSUM = 80
IN_BLOCKS = 84

INODE_TYPE_FILE = 1
INODE_TYPE_DIR = 2

DIR_ENTRY_SIZE = 8
MAX_DIR_ENTRIES = 256


class Image:
    """A nanofs image, read by this script rather than by the kernel."""

    def __init__(self, path):
        with open(path, "rb") as f:
            self.blocks = f.read((1 + mkfs.INODE_COUNT + mkfs.DATA_BLOCK_CNT)
                                 * mkfs.BLOCK_SIZE)
        self.sb = self.block(0)

    def block(self, index):
        at = index * mkfs.BLOCK_SIZE
        return self.blocks[at:at + mkfs.BLOCK_SIZE]

    def inode(self, index):
        return self.block(mkfs.INODE_START + index)

    def data(self, index):
        return self.block(mkfs.DATA_START + index)

    @staticmethod
    def u32(buf, off):
        return struct.unpack_from("<I", buf, off)[0]

    @staticmethod
    def sum_with_hole(buf, hole):
        return mkfs.crc32(buf[:hole] + b"\0\0\0\0" + buf[hole + 4:])

    def superblock_ok(self):
        return (self.u32(self.sb, 0) == mkfs.MAGIC
                and self.u32(self.sb, 4) == mkfs.VERSION
                and self.sum_with_hole(self.sb, mkfs.SB_CHECKSUM_OFFSET)
                == self.u32(self.sb, mkfs.SB_CHECKSUM_OFFSET))

    def inode_ok(self, index):
        inode = self.inode(index)
        return self.sum_with_hole(inode, IN_CHECKSUM) == self.u32(inode, IN_CHECKSUM)

    def name(self, index):
        raw = self.inode(index)[IN_NAME:IN_NAME + IN_NAME_LEN]
        return raw.split(b"\0")[0].decode("ascii", "replace")

    def contents(self, index):
        """A file's bytes, gathered from the blocks its inode names."""
        inode = self.inode(index)
        size = self.u32(inode, IN_SIZE)
        out = b""
        left = size
        i = 0
        while left > 0 and i < mkfs.MAX_BLOCKS:
            block = self.u32(inode, IN_BLOCKS + i * 4)
            take = min(left, mkfs.BLOCK_SIZE)
            out += self.data(block)[:take]
            left -= take
            i += 1
        return out

    def data_checksum_ok(self, index):
        """The per-block CRCs exclusive-ored, as the driver writes them."""
        inode = self.inode(index)
        stored = self.u32(inode, IN_DATA_CHECKSUM)
        if stored == 0:
            return True
        size = self.u32(inode, IN_SIZE)
        computed = 0
        left = size
        i = 0
        while left > 0 and i < mkfs.MAX_BLOCKS:
            block = self.u32(inode, IN_BLOCKS + i * 4)
            take = min(left, mkfs.BLOCK_SIZE)
            computed ^= mkfs.crc32(self.data(block)[:take])
            left -= take
            i += 1
        return computed == stored

    def walk(self, index=0, at="", out=None, seen=None):
        """Every path in the tree, and what each one is. A damaged image can
        name a directory from inside itself; each inode is entered once, so
        that comes out as a tree with something missing rather than a walk
        that does not end."""
        if out is None:
            out = {}
        if seen is None:
            seen = set()
        if index in seen or not self.inode_ok(index):
            return out
        seen.add(index)

        inode = self.inode(index)
        if self.u32(inode, IN_TYPE) != INODE_TYPE_DIR:
            return out

        count = min(self.u32(inode, IN_SIZE), MAX_DIR_ENTRIES)
        block = self.data(self.u32(inode, IN_BLOCKS))
        for i in range(count):
            child = self.u32(block, i * DIR_ENTRY_SIZE)
            if child >= mkfs.INODE_COUNT or child in seen or not self.inode_ok(child):
                continue
            path = at + "/" + self.name(child)
            kind = self.u32(self.inode(child), IN_TYPE)
            out[path] = (child, kind)
            if kind == INODE_TYPE_DIR:
                self.walk(child, path, out, seen)
        return out


def root_device(sh):
    """Which disk the root filesystem is on, as `mounts` names it."""
    for line in sh.run("mounts").splitlines():
        if line.strip().startswith("ext2 on /"):
            parts = line.split()
            if len(parts) >= 4:
                return parts[3]
    return None


def other_devices(sh, root):
    """Every disk that is not the root's, as `disks` names them."""
    names = []
    for line in sh.run("disks").splitlines():
        parts = line.split()
        name = parts[0] if parts else ""
        if name and name != root and not name.startswith("disk"):
            names.append(name)
    return names


def which_is_formatted(sh, disks):
    """Of two disks, the one that already carries a nanofs and the one that
    does not -- QEMU hands the virtio-mmio slots out in an order of its own,
    so they are told apart by what is on them, not by their names."""
    for disk in disks:
        out = sh.run("mount nanofs %s /pre" % disk)
        if "mounted nanofs on /pre" in out:
            return disk, [d for d in disks if d != disk]
    return None, disks


def premade(sh, disk):
    """The disk mkfs_nanofs.py formatted, mounted at /pre: the kernel reads
    a superblock and a root inode another implementation wrote, and writes a
    file into it."""
    sh.run("write /pre/from-kernel.txt written-into-a-python-made-filesystem")
    pt.check("a file written into the python-made filesystem reads back",
             "written-into-a-python-made-filesystem" in sh.run("cat /pre/from-kernel.txt"))
    sh.run("sync")
    out = sh.run("umount /pre")
    pt.check("and it unmounts", "unmounted" in out, out)


def work(sh, disk):
    """Format, mount, exercise, take down, bring back, check."""
    out = sh.run("format nanofs " + disk)
    if not pt.check("the disk formats as nanofs", "formatted" in out, out):
        return

    out = sh.run("mount nanofs %s /data" % disk)
    if not pt.check("and mounts", "mounted nanofs on /data" in out, out):
        return

    out = sh.run("mounts")
    pt.check("the mount shows the uuid the format gave it",
             re.search(r"nanofs on /data\s+uuid=[0-9a-f]{32}", out) is not None, out)

    out = sh.run("fstest /data " + FSTEST_SIZE)
    pt.check("the filesystem self-test passes on it", "fstest: passed" in out, out)

    # Something to find again after a remount
    sh.run("mkdir /data/keep")
    sh.run("write /data/keep/a.txt nanofs-survives-a-remount")
    sh.run("mkdir /data/keep/inner")
    sh.run("write /data/keep/inner/b.txt second-file")
    for i in range(BIG_LINES):
        sh.run("append /data/keep/big.txt " + BIG_LINE % i)
    out = sh.run("stat /data/keep/big.txt")
    want = BIG_LINES * len(BIG_LINE % 0)
    pt.check("the grown file is as long as what went into it",
             ("%d bytes" % want) in out, out)

    sh.run("mv /data/keep/a.txt /data/keep/renamed.txt")
    sh.run("write /data/keep/gone.txt to-be-removed")
    sh.run("rm /data/keep/gone.txt")
    sh.run("sync")

    out = sh.run("umount /data")
    if not pt.check("it unmounts", "unmounted" in out, out):
        return

    out = sh.run("mount nanofs %s /data" % disk)
    if not pt.check("and mounts again", "mounted nanofs on /data" in out, out):
        return

    pt.check("a file written before the unmount is there",
             "nanofs-survives-a-remount" in sh.run("cat /data/keep/renamed.txt"))
    pt.check("so is one a directory down",
             "second-file" in sh.run("cat /data/keep/inner/b.txt"))
    out = sh.run("stat /data/keep/big.txt")
    pt.check("and the grown one is still its full length",
             ("%d bytes" % want) in out, out)
    pt.check("what was removed is still gone",
             "not found" in sh.run("cat /data/keep/gone.txt").lower())

    listing = sh.run("ls /data/keep")
    pt.check("what is left is what should be",
             "renamed.txt" in listing and "big.txt" in listing
             and "inner" in listing and "gone.txt" not in listing, listing)

    # Removing a directory takes everything under it. This one goes; /keep
    # stays behind for the reader below to find on the disk.
    sh.run("mkdir /data/tree")
    sh.run("write /data/tree/leaf.txt leaf")
    sh.run("rm /data/tree")
    pt.check("a directory removed whole leaves nothing behind",
             "not found" in sh.run("cat /data/tree/leaf.txt").lower())

    sh.run("sync")
    out = sh.run("umount /data")
    pt.check("and it unmounts at the end", "unmounted" in out, out)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tcg", action="store_true")
    ap.add_argument("--keep", action="store_true", help="leave the images behind")
    args = ap.parse_args()

    tmp = tempfile.mkdtemp(prefix="nos-nanotest-")
    root = os.path.join(tmp, "root.img")
    nano = os.path.join(tmp, "nano.img")
    pre = os.path.join(tmp, "pre.img")
    log = os.path.join(tmp, "serial.log")
    pt.mkrootfs(root)
    blank_image(nano, NANO_DISK_MB)
    python_formatted_image(pre, NANO_DISK_MB)

    import platform
    hvf = platform.system() == "Darwin" and platform.machine() == "arm64" and not args.tcg
    accel = ["-accel", "hvf", "-cpu", "host"] if hvf else ["-accel", "tcg", "-cpu", "cortex-a72"]
    argv = ["qemu-system-aarch64", "-M", "virt,gic-version=3", "-smp", "4", "-m", "1024"] + accel + [
        "-kernel", os.path.join(ROOT, "nos-arm64.img"),
        "-append", "root=auto udpshell=%d" % SHELL_PORT,
        "-global", "virtio-mmio.force-legacy=false",
        "-drive", "file=%s,format=raw,id=hd0,if=none" % root,
        "-device", "virtio-blk-device,drive=hd0",
        "-drive", "file=%s,format=raw,id=hd1,if=none" % nano,
        "-device", "virtio-blk-device,drive=hd1",
        "-drive", "file=%s,format=raw,id=hd2,if=none" % pre,
        "-device", "virtio-blk-device,drive=hd2",
        "-device", "virtio-net-device,netdev=n0",
        "-netdev", "user,id=n0,hostfwd=udp::%d-:%d" % (SHELL_PORT, SHELL_PORT),
        "-serial", "file:" + log, "-display", "none"]

    p = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        if not pt.check("reaches the shell", pt.wait_log(log, "boot: complete", 300)):
            return 1

        sh = pt.Shell()
        root_dev = root_device(sh)
        if not pt.check("the root filesystem names its disk", root_dev is not None,
                        sh.run("mounts")):
            return 1
        disks = other_devices(sh, root_dev)
        if not pt.check("there are two more disks", len(disks) == 2, sh.run("disks")):
            return 1

        made, rest = which_is_formatted(sh, disks)
        if not pt.check("the kernel mounts the nanofs mkfs_nanofs.py wrote",
                        made is not None, sh.run("disks")):
            return 1
        premade(sh, made)

        work(sh, rest[0])

        sh.sock.settimeout(5)
        try:
            sh.run("poweroff")
        except Exception:
            pass
        for _ in range(30):
            if p.poll() is not None:
                break
            time.sleep(0.5)
    finally:
        if p.poll() is None:
            p.send_signal(signal.SIGTERM)
            try:
                p.wait(10)
            except subprocess.TimeoutExpired:
                p.kill()

    # What the kernel left on the disk, read by this script's own idea of
    # the layout: the check that would catch a field at the wrong offset,
    # which a driver reading back its own writes never notices.
    image = Image(nano)
    pt.check("the superblock this script reads is one it recognises",
             image.superblock_ok())

    tree = image.walk()
    pt.check("the tree on the disk is the one the shell made",
             set(tree) == {"/keep", "/keep/renamed.txt", "/keep/inner",
                           "/keep/inner/b.txt", "/keep/big.txt"},
             ", ".join(sorted(tree)) or "(nothing)")

    if "/keep/renamed.txt" in tree:
        idx = tree["/keep/renamed.txt"][0]
        pt.check("a file's bytes are where its inode says they are",
                 image.contents(idx) == b"nanofs-survives-a-remount",
                 repr(image.contents(idx)[:64]))
    if "/keep/big.txt" in tree:
        idx = tree["/keep/big.txt"][0]
        pt.check("the grown file's blocks are all where its inode says",
                 len(image.contents(idx)) == BIG_SIZE
                 and image.contents(idx).startswith(b"line-0000"),
                 repr(image.contents(idx)[:32]))
        pt.check("and its data checksum is the one over those blocks",
                 image.data_checksum_ok(idx))

    # Nothing must go wrong while the filesystem is in use. The shutdown that
    # follows is another matter: arm64 has a known fault in the static
    # destructors after everything is unmounted.
    text = open(log, errors="replace").read()
    up = text.split("Stopping cpu")[0]
    pt.check("nothing panicked while the filesystem was in use",
             "PANIC" not in up,
             "\n".join(l for l in up.splitlines() if "PANIC" in l))

    if args.keep:
        print("images left at " + tmp)
    else:
        subprocess.run(["rm", "-rf", tmp])

    print()
    if pt.failures:
        print("FAILED: " + ", ".join(pt.failures))
        return 1
    print("nanofs-test: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
