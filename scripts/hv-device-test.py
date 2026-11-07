#!/usr/bin/env python3
"""hv device test: a guest's disk that is a partition or a whole disk of
nos's, the filesystem out of the way (plans/hv-local-disks.md) -- named by
what finds it again from one boot to the next, given only where it may be,
and written to by `hv convert` and read by a guest through the block layer's
queue for it (`kcore::block::DiskQueue`).

Two boots of nos.iso over one root, two NVMe drives beside it:

  - A, a GPT disk of four partitions: an ext4's superblock (p1), nothing
    (p2), an EFI System partition (p3), and bytes of 0xFF (p4); B, a whole
    disk of zeros.
  - The first boot: `disks` and `hv devices` give each its id -- a
    partition its GUID, a disk its model and serial -- and say what is on
    it; `hv create` refuses the EFI System partition outright and the ext4
    without force, gives p2 to d1 and the whole of B to d3 by their ids,
    and refuses the whole of A, which overlaps d1's p2; `hv convert` refuses
    p4 for the bytes on it and pours a qcow2 image onto it with force --
    the host holds p4 to the image byte for byte after, its holes zeros
    over the 0xFF; hvboot, booted on p2 by name, reads it and finds nothing
    to boot; d1 started holds p2 -- `hv devices` says so and a write of the
    shell's to it is refused -- and stopped lets it go; a device attached to
    d3 and detached; a clone of d1 refused.
  - The second boot, the drives the other way round -- nvme0 the one nvme1
    was, as Linux and nos number the EX44's two from one boot to the next:
    the ids where they were, under the other names, and d1 started on its
    partition by its id; `hv delete d1` leaves the device as it is.

x86-64 alone: the guests are hv's, which on arm64 runs no Linux guest yet.
"""

import argparse
import hashlib
import importlib.util
import os
import random
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import uuid
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

spec = importlib.util.spec_from_file_location("hv_test", os.path.join(HERE, "hv-test.py"))
hvt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hvt)
pt = hvt.pt

SECTOR = 512
MIB = 1024 * 1024
# Drive A: four partitions of these many MiB from 1 MiB on -- p3 the EFI
# System partition -- and room for the GPT's backup at the end.
A_PARTS = ((8, "linux"), (16, "linux"), (4, "esp"), (16, "linux"))
A_MB = 64
B_MB = 64
LINUX_DATA = uuid.UUID("0fc63daf-8483-4772-8e79-3d69d8477de4")
ESP = uuid.UUID("c12a7328-f81f-11d2-ba4b-00a0c93ec93b")
# Each partition's unique GUID: made here, so the ids are known.
UNIQUE = [uuid.UUID(int=(0x6e6f7364_6576_0000_0000_000000000000 + i + 1)) for i in range(len(A_PARTS))]
# What QEMU's NVMe says it is, and the serials the drives are given.
MODEL = "QEMU_NVMe_Ctrl"
SERIAL_A = "nosdevtesta"
SERIAL_B = "nosdevtestb"
# The image poured onto p4: a disk of this many bytes, data here and there.
POUR_SIZE = 6 * MIB


def part_start(i):
    """Partition i's first sector: 1 MiB in, each after the one before."""
    return (1 + sum(mb for mb, _ in A_PARTS[:i])) * MIB // SECTOR


def drive_a(path):
    """Drive A: its GPT, p1 an ext4's superblock (magic, extents, a label),
    p2 zeros, p4 0xFF throughout."""
    total = A_MB * MIB // SECTOR
    with open(path, "wb") as f:
        f.truncate(A_MB * MIB)
    entries = bytearray(128 * 128)
    for i, (mb, kind) in enumerate(A_PARTS):
        at = i * 128
        entries[at:at + 16] = (ESP if kind == "esp" else LINUX_DATA).bytes_le
        entries[at + 16:at + 32] = UNIQUE[i].bytes_le
        entries[at + 32:at + 40] = struct.pack("<Q", part_start(i))
        entries[at + 40:at + 48] = struct.pack("<Q", part_start(i) + mb * MIB // SECTOR - 1)
    mbr = bytearray(SECTOR)
    mbr[446 + 4] = 0xEE
    mbr[446 + 8:446 + 12] = struct.pack("<I", 1)
    mbr[446 + 12:446 + 16] = struct.pack("<I", total - 1)
    mbr[510:512] = b"\x55\xaa"
    header = bytearray(92)
    header[0:8] = b"EFI PART"
    header[8:12] = struct.pack("<I", 0x00010000)
    header[12:16] = struct.pack("<I", 92)
    header[24:32] = struct.pack("<Q", 1)
    header[32:40] = struct.pack("<Q", total - 1)
    header[40:48] = struct.pack("<Q", 34)
    header[48:56] = struct.pack("<Q", total - 34)
    header[56:72] = bytes(range(16))
    header[72:80] = struct.pack("<Q", 2)
    header[80:84] = struct.pack("<I", 128)
    header[84:88] = struct.pack("<I", 128)
    header[88:92] = struct.pack("<I", zlib.crc32(bytes(entries)) & 0xFFFFFFFF)
    header[16:20] = struct.pack("<I", zlib.crc32(bytes(header)) & 0xFFFFFFFF)
    with open(path, "r+b") as f:
        f.write(mbr)
        f.write(header + bytes(SECTOR - len(header)))
        f.write(entries)
        # p1: an ext4's superblock, at 1 KiB in -- its magic, extents in
        # its incompatible features, its label.
        sb = bytearray(1024)
        sb[56:58] = struct.pack("<H", 0xEF53)
        sb[96:100] = struct.pack("<I", 0x40)
        sb[120:127] = b"devtest"
        f.seek(part_start(0) * SECTOR + 1024)
        f.write(sb)
        # p4: 0xFF, every byte.
        f.seek(part_start(3) * SECTOR)
        f.write(b"\xff" * (A_PARTS[3][0] * MIB))


def pour_image(tmp, rootdir):
    """The qcow2 image p4 is given, made by qemu-img of a raw disk with data
    here and there and holes between: the raw disk's bytes, which p4 must
    hold after."""
    rng = random.Random(23)
    raw = bytearray(POUR_SIZE)
    for _ in range(12):
        at = rng.randrange(0, POUR_SIZE - 300000) // 4096 * 4096
        n = rng.choice([4096, 65536, 200000, 300000])
        raw[at:at + n] = rng.randbytes(n)
    # An MBR at its start, one partition of Linux's: what the device is
    # seen to hold once poured, and so a VM is given it after only with
    # force.
    raw[0:512] = bytes(512)
    raw[446 + 4] = 0x83
    raw[446 + 8:446 + 16] = struct.pack("<II", 2048, POUR_SIZE // SECTOR - 2048)
    raw[510:512] = b"\x55\xaa"
    raw_path = os.path.join(tmp, "pour.raw")
    with open(raw_path, "wb") as f:
        f.write(raw)
    subprocess.run(["qemu-img", "convert", "-f", "raw", "-O", "qcow2", raw_path, os.path.join(rootdir, "pour.qcow2")],
                   check=True)
    return bytes(raw)


def spin_elf(path):
    """A PVH image that runs until it is stopped: an ELF32 of one segment at
    1 MiB -- `pause; jmp` back to it -- and the Xen note naming it the
    32-bit entry (hypervisor/hv/src/pvh.rs). d1's kernel: a guest that holds its disk
    for as long as the checks look at it, where hvboot on a disk with
    nothing on it would power itself off at once."""
    entry = 0x100000
    code = b"\xf3\x90\xeb\xfc"
    note = struct.pack("<III", 4, 4, 18) + b"Xen\0" + struct.pack("<I", entry)
    ehdr = b"\x7fELF" + bytes([1, 1, 1]) + bytes(9) + struct.pack("<HHIIIIIHHHHHH", 2, 3, 1, entry, 52, 0, 0, 52,
                                                                     32, 2, 0, 0, 0)
    note_at = 52 + 2 * 32
    code_at = 0x1000
    load = struct.pack("<IIIIIIII", 1, code_at, entry, entry, len(code), len(code), 5, 0x1000)
    pnote = struct.pack("<IIIIIIII", 4, note_at, 0, 0, len(note), len(note), 4, 4)
    image = bytearray(code_at + len(code))
    image[:len(ehdr)] = ehdr
    image[52:52 + 32] = load
    image[84:84 + 32] = pnote
    image[note_at:note_at + len(note)] = note
    image[code_at:] = code
    with open(path, "wb") as f:
        f.write(image)


def id_a(i):
    return "part:" + str(UNIQUE[i])


ID_B = "disk:nvme-%s_%s" % (MODEL, SERIAL_B)

# The first boot's script, past insmod and hv on.
FIRST = [
    "disks",                                                    # 0
    "hv devices",                                               # 1
    "hv create e1 /hvboot mem=64 disk=dev:nvme03",              # 2
    "hv create e2 /hvboot mem=64 disk=dev:nvme01",              # 3
    "hv create d1 /spin.elf mem=64 disk=dev:nvme02",            # 4
    "cat /vms/d1/vm.toml",                                      # 5
    "hv create d2 /hvboot mem=64 disk=dev:nvme0",               # 6
    "hv create d3 /hvboot mem=64 disk=dev:nvme1",               # 7
    "cat /vms/d3/vm.toml",                                      # 8
    "hv convert /pour.qcow2 dev:nvme04",                        # 9
    "hv convert /pour.qcow2 dev:nvme04 force",                  # 10
    "hv boot /hvboot mem=64 secs=60 disk=dev:nvme02",           # 11
    "hv start d1",                                              # 12
    "hv devices",                                               # 13
    "diskwrite nvme02 0 55",                                    # 14
    "hv stop d1 secs=10",                                       # 15
    "hv disk attach d3 dev:nvme04",                             # 16
    "hv disk attach d3 dev:nvme04 force",                       # 17
    "cat /vms/d3/vm.toml",                                      # 18
    "hv disk detach d3 dev:nvme04",                             # 19
    "hv clone d1 d4",                                           # 20
    "hv devices",                                               # 21
]

# The second's, the drives the other way round.
SECOND = [
    "disks",                                                    # 0
    "hv devices",                                               # 1
    "hv start d1",                                              # 2
    "hv list",                                                  # 3
    "hv devices",                                               # 4
    "hv stop d1 secs=10",                                       # 5
    "hv delete d1",                                             # 6
]


def script(body):
    return ["insmod /hv.ko", "hv on"] + body + [hvt.RC_LAST, "poweroff"]


def boot(tmp, root, a, b, swapped, body, kvm, name):
    """nos.iso over `root`, A and B beside it -- B first with `swapped` --
    running `body` and powering off: what each command of `body` printed,
    by its place in it; None when the boot did not get there."""
    log = os.path.join(tmp, "serial-%s.log" % name)
    drives = [("a", a, SERIAL_A), ("b", b, SERIAL_B)]
    if swapped:
        drives.reverse()
    argv = ["qemu-system-x86_64", "-display", "none", "-m", "1G", "-no-reboot",
            "-smp", "%d,sockets=1,cores=%d,threads=2" % (hvt.CPUS, hvt.CPUS // 2),
            "-cpu", "host" if kvm else "max",
            "-cdrom", os.path.join(ROOT, "nos.iso"), "-serial", "file:" + log,
            "-drive", "file=%s,format=raw,id=root,if=none" % root,
            "-device", "virtio-blk-pci,drive=root,disable-legacy=on,disable-modern=off"]
    for tag, path, serial in drives:
        argv += ["-drive", "file=%s,format=raw,id=%s,if=none,discard=unmap" % (path, tag),
                 "-device", "nvme,serial=%s,drive=%s" % (serial, tag)]
    if kvm:
        argv += ["-enable-kvm"]
    p = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        if not pt.check("%s: boots" % name, pt.wait_log(log, "boot: complete", 400)):
            return None
        if not pt.check("%s: the boot script runs to the end" % name, pt.wait_log(log, "> " + hvt.RC_LAST, 1200)):
            return None
        hvt.wait_done(log, 600)
        # Powered off: the root unmounted clean, for the next boot's script
        # to be put on it.
        try:
            p.wait(180)
        except subprocess.TimeoutExpired:
            pt.check("%s: powers off" % name, False)
        blocks = hvt.blocks(log)
        full = script(body)
        cmds = [c for c, _ in blocks]
        if not pt.check("%s: every command of the script ran, in order" % name, cmds[:len(full) - 1] == full[:-1],
                        "\n".join(cmds)):
            return None
        return [t for _, t in blocks[2:2 + len(body)]]
    finally:
        pt.kill(p)
        if os.path.exists(log):
            shutil.copy(log, os.path.join(tmp, "kept-%s.log" % name))


def put_rc(tmp, root, lines):
    """The root's /etc/rc made `lines`: debugfs over the image, natively or
    in the nos-builder image as mkrootfs.sh runs mke2fs."""
    rc = os.path.join(tmp, "rc.next")
    with open(rc, "w") as f:
        f.write("# scripts/hv-device-test.py\n" + "\n".join(lines) + "\n")
    cmds = ["rm /etc/rc", "write %s /etc/rc"]
    if shutil.which("debugfs"):
        for c in cmds:
            subprocess.run(["debugfs", "-w", "-R", c % rc if "%s" in c else c, root], check=True,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        return
    d = os.path.dirname(os.path.abspath(root))
    base = os.path.basename(root)
    inner = "debugfs -w -R 'rm /etc/rc' /w/%s && debugfs -w -R 'write /w/rc.next /etc/rc' /w/%s" % (base, base)
    subprocess.run(["docker", "run", "--platform", "linux/amd64", "--rm", "-v", "%s:/w" % d, "nos-builder",
                    "bash", "-c", inner], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def line_of(text, name):
    """`name`'s line in a listing of devices -- `disks`, `hv devices`."""
    for line in text.splitlines():
        if line.split()[:1] == [name]:
            return line
    return ""


def first_checks(o, a, image):
    """The first boot's: `o[i]` what FIRST[i] printed."""
    disks = o[0]
    for i in range(len(A_PARTS)):
        pt.check("disks gives nvme0%d its GUID as its id" % (i + 1), id_a(i) in line_of(disks, "nvme0%d" % (i + 1)),
                 disks)
    pt.check("disks gives the whole of B its model and serial as its id", ID_B in line_of(disks, "nvme1"), disks)
    dev = o[1]
    pt.check("hv devices: p1 holds an ext4, its label said",
             'holds an ext4 filesystem labelled "devtest"' in line_of(dev, "nvme01"), dev)
    pt.check("hv devices: p2 and B empty", "empty" in line_of(dev, "nvme02") and "empty" in line_of(dev, "nvme1"), dev)
    pt.check("hv devices: p3 an EFI System partition", "an EFI System partition" in line_of(dev, "nvme03"), dev)
    pt.check("hv devices: p4's bytes something nos does not recognize",
             "data nos does not recognize" in line_of(dev, "nvme04"), dev)
    pt.check("hv devices: the root held by its mount", "held by" in line_of(dev, "vda"), dev)
    pt.check("an EFI System partition refused outright", "the machine boots from it" in o[2], o[2])
    pt.check("the ext4 refused without force, what is on it said",
             "holds an ext4 filesystem" in o[3] and "force" in o[3] and "written" not in o[3], o[3])
    pt.check("p2 given to d1", "written" in o[4], o[4])
    pt.check("d1's vm.toml names p2 by its id, not its name", id_a(1) in o[5] and "nvme02" not in o[5], o[5])
    pt.check("the whole of A refused: it overlaps d1's p2", "vm d1's disk already" in o[6], o[6])
    pt.check("the whole of B given to d3", "written" in o[7], o[7])
    pt.check("d3's vm.toml names B by its model and serial", ID_B in o[8], o[8])
    pt.check("hv convert refuses p4 for the bytes on it", "does not recognize" in o[9] and "force" in o[9], o[9])
    pt.check("hv convert pours the image onto p4 with force", "MiB written" in o[10], o[10])
    with open(a, "rb") as f:
        f.seek(part_start(3) * SECTOR)
        got = f.read(POUR_SIZE)
    pt.check("p4 holds the image byte for byte, its holes zeros over the 0xFF", got == image,
             "first difference at byte %s" % next((i for i in range(POUR_SIZE) if got[i:i + 1] != image[i:i + 1]), None))
    pt.check("hvboot, booted on p2 by name, reads it and finds nothing to boot",
             "cannot boot" in o[11] and "the guest powered itself off" in o[11], o[11])
    pt.check("d1 starts on p2", re.search(r"hv: vm \d+ \(d1\) started", o[12]) is not None, o[12])
    pt.check("hv devices: p2 held by d1 while it runs", "held by vm d1's disk" in line_of(o[13], "nvme02"), o[13])
    pt.check("a write of the shell's to p2 is refused while d1 has it", "in use by vm d1's disk" in o[14], o[14])
    pt.check("p4 refused to d3 without force: the image is on it", "force" in o[16] and "now" not in o[16], o[16])
    pt.check("p4 attached to d3 with force", "d3's disk now" in o[17], o[17])
    pt.check("d3's vm.toml names p4 too", id_a(3) in o[18], o[18])
    pt.check("p4 detached from d3, left as it is", "taken off d3's vm.toml" in o[19], o[19])
    pt.check("a clone of d1 refused: its disk is a device", "a device of nos's" in o[20], o[20])
    pt.check("hv devices: p2 let go once d1 stopped", "held by" not in line_of(o[21], "nvme02"), o[21])


def second_checks(o):
    """The second boot's, the drives the other way round: B is nvme0 and A
    nvme1 now."""
    disks = o[0]
    for i in range(len(A_PARTS)):
        pt.check("the other way round: nvme1%d has the GUID nvme0%d had" % (i + 1, i + 1),
                 id_a(i) in line_of(disks, "nvme1%d" % (i + 1)), disks)
    pt.check("the other way round: nvme0 is B, by its serial", ID_B in line_of(disks, "nvme0"), disks)
    pt.check("d1 starts on its partition by its id, its name nvme12 now",
             re.search(r"hv: vm \d+ \(d1\) started", o[2]) is not None, o[2])
    pt.check("hv list: d1 runs", re.search(r"\bd1\b.*running", o[3]) is not None, o[3])
    pt.check("hv devices: nvme12 held by d1", "held by vm d1's disk" in line_of(o[4], "nvme12"), o[4])
    pt.check("hv delete d1 leaves its device as it is", "left as it is" in o[6] and id_a(1) in o[6], o[6])


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tcg", action="store_true", help="do not use KVM")
    ap.add_argument("--keep", action="store_true", help="keep the logs and drives")
    args = ap.parse_args()
    if shutil.which("qemu-img") is None:
        sys.exit("hv-device-test needs qemu-img")
    kvm = os.path.exists("/dev/kvm") and not args.tcg and (hvt.host_has_svm() or hvt.host_has_vmx())
    version = hvt.qemu_version("qemu-system-x86_64")
    if not kvm and (version is None or version < hvt.TCG_REAL_MODE):
        sys.exit("QEMU under TCG: %d.%d or later wanted" % hvt.TCG_REAL_MODE)

    tmp = tempfile.mkdtemp(prefix="nos-hvdev-")
    try:
        staged = os.path.join(tmp, "staged")
        os.makedirs(staged)
        image = pour_image(tmp, staged)
        a = os.path.join(tmp, "a.img")
        b = os.path.join(tmp, "b.img")
        drive_a(a)
        with open(b, "wb") as f:
            f.truncate(B_MB * MIB)
        root, _ = hvt.rootfs(tmp, "x86_64", script(FIRST))
        # The image to pour, put in the root: rootfs staged it already.
        shutil.copy(os.path.join(staged, "pour.qcow2"), os.path.join(tmp, "rootdir", "pour.qcow2"))
        spin_elf(os.path.join(tmp, "rootdir", "spin.elf"))
        subprocess.run([os.path.join(HERE, "mkrootfs.sh"), root, "64", os.path.join(tmp, "rootdir"), "1024"],
                       cwd=ROOT, check=True, stdout=subprocess.DEVNULL)
        o = boot(tmp, root, a, b, False, FIRST, kvm, "first")
        if o is not None:
            first_checks(o, a, image)
            put_rc(tmp, root, script(SECOND))
            o = boot(tmp, root, a, b, True, SECOND, kvm, "second")
            if o is not None:
                second_checks(o)
    finally:
        if args.keep:
            print("kept: " + tmp)
        else:
            shutil.rmtree(tmp, ignore_errors=True)
    print()
    if pt.failures:
        print("FAILED: " + ", ".join(pt.failures))
        return 1
    print("hv-device-test: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
