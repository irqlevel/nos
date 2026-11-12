#!/usr/bin/env python3
"""hvboot test: what hvboot boots off a guest's disk, decided on the host.

hvboot (src/crates/hypervisor/hvboot) is the loader hv starts a guest in by PVH; it reads
the guest's own partition table, its ext2/3/4 /boot and its GRUB
configuration, and boots the entry GRUB would. What it boots is its `plan`'s
to work out -- src/crates/hypervisor/hvboot/src/plan.rs, with no platform in
it -- and fuzz/hvboot compiles that, and the readers it reads through
({gpt,ext,grub,bzimage}.rs over extfmt and the tree's CRCs), as they are,
into a host program: the boot's own rules, not a second copy of them, run
under hvboot's own heap -- arena.rs's arithmetic, no bigger than its heap --
over disks made here:

  - GPT with a /boot of its own (an XBOOTLDR partition, as Ubuntu's image
    has), MBR with /boot on the root filesystem (as Debian's), and a disk
    that is one filesystem whole; ext4 with extents, ext3, and ext2 with
    1 KiB blocks, whose kernel runs into the double-indirect blocks;
  - each with a grub.cfg written as grub-mkconfig writes Debian's and
    Ubuntu's -- conditionals, a submenu, `search --fs-uuid`,
    `$menuentry_id_option` -- and the cases GRUB chooses among: no grubenv
    (its default), grubenv's next_entry into the submenu, GRUB_DEFAULT=saved
    with saved_entry naming an entry by its id, and `hvboot.entry=` naming
    one;
  - and next_entry booted once: the image written as hvboot writes a
    guest's disk, the variable emptied in grubenv's block in place -- the
    others as they were, the block GRUB's still, the filesystem clean to
    e2fsck -- and the boot after taking the default;

and holds it to the entry GRUB would boot, the command line it would give,
and the kernel and initrds -- two of them one after another at GRUB's
alignment, the bytes between them zeros in RAM that held other bytes --
read byte for byte as they were written. Then:

  - grub.cfg read as GRUB 2.06 reads it, each case's answer found by booting
    GRUB itself (grub-mkrescue's i386-pc GRUB under QEMU, the kernel saying
    the command line it was given): the command line's quoting and the
    splitting of a variable's value, `default` by number (strtoul's), title,
    `>>` and into submenus within submenus, braces a token of their own and
    an escaped one not, assignments without `set`, a `linux` unloading the
    initrd before it, `search`'s forms, menuentry's options, grubenv's
    escapes and its signature;
  - what it says when it cannot boot: a filesystem refused for a feature,
    by name; a partition past the disk's end; a grubenv GRUB would not read;
    `hvboot.disk=` past the disks; a grub.cfg longer than it reads; and a
    grub.cfg that could not be looked for, which ends the search rather
    than another filesystem's being booted;
  - a root set outside every entry, the entries' files on another
    filesystem than grub.cfg;
  - its heap: a grub.cfg as long as hvboot reads, of each shape that costs
    its parse the most a byte, and an initrd whose extent tree is two
    levels deep, each booted inside hvboot's heap and all of it given back;
    and the heap's arithmetic held to a model of it;

and each disk is fuzzed: its metadata -- partition table, superblock,
descriptors, inodes, extent and directory blocks, grub.cfg -- changed at
random, a disk at a time, and the code held to never panicking, never
reading outside the disk, always ending, staying inside its heap and giving
all of it back.

    python3 scripts/hvboot-test.py                      # the gate
    python3 scripts/hvboot-test.py --iterations 50000   # a longer campaign
    python3 scripts/hvboot-test.py --image ubuntu.raw   # a distribution's, made raw

The partition tables are written here, by the UEFI specification's layout
and the MBR's; the filesystems by mke2fs and debugfs. Needs cargo and
e2fsprogs (mke2fs, e2fsck, debugfs) on the host -- Linux; on a Mac, in a
container. Exit code 0 = every check passed.
"""

import argparse
import os
import random
import shutil
import subprocess
import sys
import tempfile
import uuid
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
FUZZ = os.path.join(ROOT, "fuzz", "hvboot")
OUT = os.path.join(ROOT, "out", "hvboot-test")
BIN = os.path.join(OUT, "release", "hvboot-test")

XBOOTLDR = "BC13C2FF-59E6-4262-A352-B275FD6F7172"
LINUX_ROOT = "4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709"
ESP = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"

failures = []


def check(what, ok, detail=""):
    print("%s: %s" % ("PASS" if ok else "FAIL", what))
    if not ok:
        failures.append(what)
        if detail:
            print("  " + detail.strip().replace("\n", "\n  ")[-2000:])
    return ok


def bzimage(rng, size):
    """A file a bzImage's header parses out of: the boot flag, HdrS,
    protocol 2.15, a 64-bit entry, and the rest the input's bytes."""
    b = bytearray(rng.randbytes(size))
    b[0x1F1] = 27                                   # setup_sects
    b[0x1FE:0x200] = (0xAA55).to_bytes(2, "little")
    b[0x201] = 0x6A                                 # the header's length past 0x202
    b[0x202:0x206] = b"HdrS"
    b[0x206:0x208] = (0x020F).to_bytes(2, "little")
    b[0x22C:0x230] = (0x7FFFFFFF).to_bytes(4, "little")
    b[0x236:0x238] = (0x7F).to_bytes(2, "little")   # xloadflags: a 64-bit entry
    b[0x238:0x23C] = (2047).to_bytes(4, "little")   # cmdline_size
    b[0x258:0x260] = (0x1000000).to_bytes(8, "little")
    b[0x260:0x264] = (64 << 20).to_bytes(4, "little")
    return bytes(b)


def loaded(kernel):
    """What of a bzImage is read into RAM: its 64-bit part, after the setup
    sectors (setup_sects, 0 meaning 4) and the boot sector."""
    return kernel[((kernel[0x1F1] or 4) + 1) * 512:]


def grub_cfg(uuid, root_prefix, saved):
    """A grub.cfg as grub-mkconfig writes Ubuntu's: `root_prefix` is where
    the kernels are on the filesystem it searches for -- "" on a /boot of
    its own, "/boot" on the root filesystem."""
    default = 'set default="${saved_entry}"' if saved else '''if [ "${next_entry}" ] ; then
   set default="${next_entry}"
   set next_entry=
   save_env next_entry
   set boot_once=true
else
   set default="0"
fi'''
    def entry(title, id_, kernel, args, initrds):
        return '''menuentry '%s' --class ubuntu --class gnu-linux $menuentry_id_option '%s' {
\trecordfail
\tload_video
\tgfxmode $linux_gfx_mode
\tinsmod gzio
\tif [ x$grub_platform = xxen ]; then insmod xzio; insmod lzopio; fi
\tinsmod part_gpt
\tinsmod ext2
\tif [ x$feature_platform_search_hint = xy ]; then
\t  search --no-floppy --fs-uuid --set=root --hint-bios=hd0,gpt1 --hint-efi=hd0,gpt1  %s
\telse
\t  search --no-floppy --fs-uuid --set=root %s
\tfi
\tlinux\t%s/%s %s
\tinitrd\t%s
}
''' % (title, id_, uuid, uuid, root_prefix, kernel, args,
       " ".join("%s/%s" % (root_prefix, i) for i in initrds))
    return '''#
# DO NOT EDIT THIS FILE
#
if [ -s $prefix/grubenv ]; then
  set have_grubenv=true
  load_env
fi
%s
if [ x"${feature_menuentry_id}" = xy ]; then
  menuentry_id_option="--id"
else
  menuentry_id_option=""
fi
export menuentry_id_option
function load_video {
  if [ x$feature_all_video_module = xy ]; then
    insmod all_video
  fi
}
set timeout=0
%s
submenu 'Advanced options for Ubuntu' $menuentry_id_option 'gnulinux-advanced-%s' {
%s%s%s}
if [ "$grub_platform" = "efi" ]; then
menuentry 'UEFI Firmware Settings' $menuentry_id_option 'uefi-firmware' {
\tfwsetup
}
fi
''' % (default,
       entry("Ubuntu", "gnulinux-simple-%s" % uuid, "vmlinuz-6.9.0-2-generic",
             "root=LABEL=cloudimg-rootfs ro console=tty1 console=ttyS0", ["initrd.img-6.9.0-2-generic"]),
       uuid,
       entry("Ubuntu, with Linux 6.9.0-2-generic", "gnulinux-6.9.0-2-advanced-%s" % uuid, "vmlinuz-6.9.0-2-generic",
             "root=LABEL=cloudimg-rootfs ro console=ttyS0", ["initrd.img-6.9.0-2-generic"]),
       entry("Ubuntu, with Linux 6.9.0-2-generic (recovery mode)", "gnulinux-6.9.0-2-recovery-%s" % uuid,
             "vmlinuz-6.9.0-2-generic", "root=LABEL=cloudimg-rootfs ro recovery nomodeset",
             ["initrd.img-6.9.0-2-generic"]),
       entry("Ubuntu, with Linux 6.8.0-1-generic", "gnulinux-6.8.0-1-advanced-%s" % uuid, "vmlinuz-6.8.0-1-generic",
             "root=LABEL=cloudimg-rootfs ro console=ttyS0 quiet", ["microcode.cpio", "initrd.img-6.8.0-1-generic"]))


ENV_SIGNATURE = b"# GRUB Environment Block\n"


def grubenv(pairs, warning=False):
    """grubenv as grub-editenv writes it: 1024 bytes, '#'-padded -- and the
    line it puts under the signature, with `warning`."""
    body = ENV_SIGNATURE.decode() \
        + ("# WARNING: Do not edit this file by tools other than grub-editenv!!!\n" if warning else "") \
        + "".join("%s=%s\n" % kv for kv in pairs)
    return (body + "#" * (1024 - len(body))).encode()


SECTOR = 512
GPT_ENTRIES = 128
GPT_ENTRY_BYTES = 128


def write_gpt(path, sectors, parts, rng):
    """A GPT as the UEFI specification lays one out, primary and backup, CRCs
    and all, over a protective MBR: `parts` is (first, count, type GUID)."""
    disk_guid = uuid.UUID(int=rng.getrandbits(128)).bytes_le
    entries = bytearray(GPT_ENTRIES * GPT_ENTRY_BYTES)
    for i, (first, count, kind) in enumerate(parts):
        e = memoryview(entries)[i * GPT_ENTRY_BYTES:(i + 1) * GPT_ENTRY_BYTES]
        e[0:16] = uuid.UUID(kind).bytes_le
        e[16:32] = uuid.UUID(int=rng.getrandbits(128)).bytes_le
        e[32:40] = first.to_bytes(8, "little")
        e[40:48] = (first + count - 1).to_bytes(8, "little")
        name = ("part%d" % (i + 1)).encode("utf-16-le")
        e[56:56 + len(name)] = name
    array_sectors = len(entries) // SECTOR

    def header(mine, other, array):
        h = bytearray(92)
        h[0:8] = b"EFI PART"
        h[8:12] = (0x00010000).to_bytes(4, "little")
        h[12:16] = (92).to_bytes(4, "little")
        h[24:32] = mine.to_bytes(8, "little")
        h[32:40] = other.to_bytes(8, "little")
        h[40:48] = (2 + array_sectors).to_bytes(8, "little")
        h[48:56] = (sectors - 2 - array_sectors).to_bytes(8, "little")
        h[56:72] = disk_guid
        h[72:80] = array.to_bytes(8, "little")
        h[80:84] = GPT_ENTRIES.to_bytes(4, "little")
        h[84:88] = GPT_ENTRY_BYTES.to_bytes(4, "little")
        h[88:92] = zlib.crc32(entries).to_bytes(4, "little")
        h[16:20] = zlib.crc32(h).to_bytes(4, "little")
        return bytes(h) + bytes(SECTOR - 92)

    mbr = bytearray(SECTOR)
    mbr[0x1BE + 4] = 0xEE
    mbr[0x1BE + 8:0x1BE + 12] = (1).to_bytes(4, "little")
    mbr[0x1BE + 12:0x1BE + 16] = min(sectors - 1, 0xFFFFFFFF).to_bytes(4, "little")
    mbr[510:512] = b"\x55\xAA"
    with open(path, "r+b") as f:
        f.write(mbr)
        f.write(header(1, sectors - 1, 2))
        f.write(entries)
        f.seek((sectors - 1 - array_sectors) * SECTOR)
        f.write(entries)
        f.write(header(sectors - 1, 1, sectors - 1 - array_sectors))


def write_mbr(path, first, count, kind):
    """A plain MBR, its one primary partition at `first`, `count` sectors."""
    mbr = bytearray(SECTOR)
    e = 0x1BE
    mbr[e + 4] = kind
    mbr[e + 8:e + 12] = first.to_bytes(4, "little")
    mbr[e + 12:e + 16] = count.to_bytes(4, "little")
    mbr[510:512] = b"\x55\xAA"
    with open(path, "r+b") as f:
        f.write(mbr)


class Disk:
    def __init__(self, name, path, files, first=0, sectors=0):
        self.name = name
        self.path = path
        self.files = files
        # Where its filesystem is, in sectors.
        self.first = first
        self.sectors = sectors


def new_uuid(rng):
    return "%08x-%04x-%04x-%04x-%012x" % (rng.getrandbits(32), rng.getrandbits(16), rng.getrandbits(16),
                                          rng.getrandbits(16), rng.getrandbits(48))


def make_disk(tmp, name, layout, fs, rng, saved=False, env=lambda uuid: [], warning=False):
    """A disk of `layout` -- gpt-boot (a /boot partition of its own), mbr
    (/boot on the root filesystem) or whole (no table) -- with a filesystem
    of type `fs`, its kernels, initrds and grub.cfg, and a grubenv of
    `env(uuid)`'s pairs when there are any: an entry's id has the
    filesystem's UUID in it."""
    uuid = new_uuid(rng)
    content = os.path.join(tmp, name + ".d")
    on_root = layout == "mbr"
    boot = os.path.join(content, "boot") if on_root else content
    os.makedirs(os.path.join(boot, "grub"))
    files = {
        "vmlinuz-6.9.0-2-generic": bzimage(rng, 6 << 20),
        "initrd.img-6.9.0-2-generic": rng.randbytes((3 << 20) + 13),
        "vmlinuz-6.8.0-1-generic": bzimage(rng, (2 << 20) + 5000),
        "initrd.img-6.8.0-1-generic": rng.randbytes(700 * 1024),
        "microcode.cpio": rng.randbytes(1001),
    }
    for n, data in files.items():
        with open(os.path.join(boot, n), "wb") as f:
            f.write(data)
    os.symlink("vmlinuz-6.9.0-2-generic", os.path.join(boot, "vmlinuz"))
    with open(os.path.join(boot, "grub", "grub.cfg"), "w") as f:
        f.write(grub_cfg(uuid, "/boot" if on_root else "", saved))
    pairs = env(uuid)
    if pairs:
        with open(os.path.join(boot, "grub", "grubenv"), "wb") as f:
            f.write(grubenv(pairs, warning))

    disk = os.path.join(tmp, name + ".raw")
    size_mib = 64
    with open(disk, "wb") as f:
        f.truncate(size_mib << 20)
    total = (size_mib << 20) // SECTOR
    first, sectors = 0, total
    if layout == "gpt-boot":
        # An ESP first, then /boot: as Ubuntu's image has them.
        first, sectors = 6144, total - 6144 - 2048
        write_gpt(disk, total, [(2048, 4096, ESP), (first, sectors, XBOOTLDR)], rng)
    elif layout == "mbr":
        first, sectors = 2048, total - 2048
        write_mbr(disk, first, sectors, 0x83)
    blocks = {"ext2": 1024, "ext3": 4096, "ext4": 4096}[fs]
    count = sectors * SECTOR // blocks
    subprocess.run(["mke2fs", "-q", "-F", "-t", fs, "-b", str(blocks), "-U", uuid, "-L", "BOOT",
                    "-E", "offset=%d" % (first * 512), "-d", content, disk, str(count)], check=True)
    return Disk(name, disk, files, first, sectors)


def populate(content, tree):
    """`tree` -- {path: bytes or text} -- written under `content`."""
    for path, data in tree.items():
        full = os.path.join(content, path.lstrip("/"))
        os.makedirs(os.path.dirname(full), exist_ok=True)
        with open(full, "wb") as f:
            f.write(data if isinstance(data, bytes) else data.encode())


def fs_disk(tmp, name, trees, rng, uuids=None, extra=(), table=True, size_mib=64):
    """A disk of an ext4 filesystem a tree -- `trees` a list of {path: bytes
    or text} -- each a GPT partition of Linux's type, in order, or with no
    `table` one filesystem whole; `uuids` theirs, made up when not given.
    The disk's path, the filesystems' UUIDs, and where each is (first
    sector, sectors)."""
    disk = os.path.join(tmp, name + ".raw")
    with open(disk, "wb") as f:
        f.truncate(size_mib << 20)
    total = (size_mib << 20) // SECTOR
    if table:
        each = (total - 4096) // len(trees) // 2048 * 2048
        parts = [(2048 + i * each, each) for i in range(len(trees))]
        write_gpt(disk, total, [(f, n, LINUX_ROOT) for f, n in parts], rng)
    else:
        parts = [(0, total)]
    uuids = list(uuids or [new_uuid(rng) for _ in trees])
    for i, ((first, count), tree) in enumerate(zip(parts, trees)):
        content = os.path.join(tmp, "%s.%d.d" % (name, i))
        os.makedirs(content)
        populate(content, tree)
        subprocess.run(["mke2fs", "-q", "-F", "-t", "ext4", "-b", "4096", "-U", uuids[i], "-L", "P%d" % (i + 1),
                        *extra, "-E", "offset=%d" % (first * SECTOR), "-d", content, disk,
                        str(count * SECTOR // 4096)], check=True)
    return disk, uuids, parts


def filesystem_of(disk, tmp):
    """The disk's filesystem, copied out to a file of its own: what e2fsck
    and debugfs are given."""
    path = os.path.join(tmp, disk.name + ".fs")
    with open(disk.path, "rb") as f:
        f.seek(disk.first * SECTOR)
        data = f.read(disk.sectors * SECTOR)
    with open(path, "wb") as f:
        f.write(data)
    return path


def harness(*argv):
    """hvboot-test with `argv`: what it printed, and its exit code."""
    r = subprocess.run([BIN] + [str(a) for a in argv], capture_output=True, text=True)
    return r.stdout + r.stderr, r.returncode


def run(disk, entry=None, write=False, path=None, cmdline=None):
    """hvboot-test image: its report, and the kernel and initrd it read --
    the image open to be written, as hvboot finds a guest's disk, with
    `write`; the loader's command line `cmdline`, as hv gives it."""
    path = path or disk.path
    dump = path + ".dump"
    os.makedirs(dump, exist_ok=True)
    argv = [BIN, "image", path, "--dump", dump] + (["--entry", entry] if entry else []) \
        + (["--write"] if write else []) + (["--cmdline", cmdline] if cmdline else [])
    r = subprocess.run(argv, capture_output=True, text=True)
    out = r.stdout + r.stderr
    kernel = initrd = b""
    if r.returncode == 0:
        with open(os.path.join(dump, "kernel"), "rb") as f:
            kernel = f.read()
        with open(os.path.join(dump, "initrd"), "rb") as f:
            initrd = f.read()
    return out, kernel, initrd


def joined(files, names):
    """Initrds one after another, each at a 4-byte boundary -- as GRUB, and
    hvboot, lay them out, the bytes between them zeros."""
    b = bytearray()
    for n in names:
        while len(b) % 4:
            b.append(0)
        b += files[n]
    return bytes(b)


def layouts(args, tmp, rng):
    """Every layout and filesystem, grub-mkconfig's grub.cfg on each and the
    entries GRUB would choose. The disks made, to fuzz."""
    disks = []
    for layout, fs in (("gpt-boot", "ext4"), ("mbr", "ext4"), ("whole", "ext2"), ("mbr", "ext3"),
                       ("gpt-boot", "ext2")):
        name = "%s-%s" % (layout, fs)
        plain = make_disk(tmp, name, layout, fs, rng)
        disks.append(plain)
        f = plain.files
        new_k, new_i = "vmlinuz-6.9.0-2-generic", ["initrd.img-6.9.0-2-generic"]
        old_k, old_i = "vmlinuz-6.8.0-1-generic", ["microcode.cpio", "initrd.img-6.8.0-1-generic"]
        prefix = "/boot" if layout == "mbr" else ""

        out, kernel, initrd = run(plain)
        check("%s: no grubenv -- the default, the first entry" % name,
              '"Ubuntu", the default, 0' in out and kernel == loaded(f[new_k]) and initrd == joined(f, new_i), out)
        check("%s: and GRUB's command line, BOOT_IMAGE first" % name,
              "command line: BOOT_IMAGE=%s/%s root=LABEL=cloudimg-rootfs ro console=tty1 console=ttyS0\n"
              % (prefix, new_k) in out, out)
        out, kernel, initrd = run(plain, "1>1")
        check("%s: hvboot.entry=1>1, the submenu's recovery entry" % name,
              "(recovery mode)" in out and "recovery nomodeset" in out and kernel == loaded(f[new_k]), out)
        # The loader's own line: an entry named by titles with spaces in
        # them, quoted as Linux quotes a word; and what hv gives the kernel
        # after the entry's own words, as hv gave it.
        out, kernel, initrd = run(plain, cmdline='console=ttyS0 hvboot.entry="Advanced options for Ubuntu>Ubuntu, '
                                                 'with Linux 6.8.0-1-generic" ip=10.0.0.2 "x=a  b"')
        check("%s: hvboot.entry= naming titles with spaces, hv's words after the entry's as they were" % name,
              kernel == loaded(f[old_k]) and "command line: BOOT_IMAGE=%s/%s root=LABEL=cloudimg-rootfs ro "
              "console=ttyS0 quiet console=ttyS0 ip=10.0.0.2 \"x=a  b\"\n" % (prefix, old_k) in out, out)

        nxt = make_disk(tmp, name + "-next", layout, fs, rng, env=lambda uuid: [("next_entry", "1>2")])
        out, kernel, initrd = run(nxt)
        f = nxt.files
        check("%s: grubenv's next_entry 1>2 -- the older kernel, two initrds joined, zeros between" % name,
              "next_entry, 1>2" in out and kernel == loaded(f[old_k]) and initrd == joined(f, old_i), out)

        # Booted once, as GRUB boots it: the image written as hvboot writes
        # a guest's disk, next_entry is emptied where grubenv's block lies,
        # and the boot after takes the default. The block as grub-editenv
        # writes it, its warning under the signature kept.
        env = [("saved_entry", "gnulinux-advanced-x"), ("next_entry", "1>2"), ("tricky", "a\\\\b c=d")]
        once = make_disk(tmp, name + "-once", layout, fs, rng, env=lambda uuid: env, warning=True)
        before = open(once.path, "rb").read()
        out, kernel, initrd = run(once, write=True)
        check("%s: next_entry booted, then emptied on the disk as GRUB empties it" % name,
              "next_entry, 1>2" in out and "next_entry cleared" in out and kernel == loaded(once.files[old_k]), out)
        out, kernel, initrd = run(once)
        check("%s: and the boot after it takes the default, the first entry" % name,
              '"Ubuntu", the default, 0' in out and kernel == loaded(once.files[new_k]), out)
        fs_path = filesystem_of(once, tmp)
        r = subprocess.run(["e2fsck", "-fn", fs_path], capture_output=True, text=True)
        check("%s: e2fsck finds the filesystem clean after the write" % name, r.returncode == 0,
              r.stdout + r.stderr)
        r = subprocess.run(["debugfs", "-R", "cat %s/grub/grubenv" % prefix, fs_path], capture_output=True)
        want = grubenv([("saved_entry", "gnulinux-advanced-x"), ("next_entry", ""), ("tricky", "a\\\\b c=d")],
                       warning=True)
        check("%s: grubenv is GRUB's block, next_entry empty, the warning and every other variable as they were"
              % name, r.stdout == want, "%r\nwanted %r" % (r.stdout, want))
        after = open(once.path, "rb").read()
        changed = [i for i in range(len(before)) if before[i] != after[i]]
        check("%s: nothing written but grubenv's bytes" % name,
              len(changed) > 0 and changed[-1] - changed[0] < 1024, "%d bytes changed" % len(changed))
        ro = make_disk(tmp, name + "-once-ro", layout, fs, rng, env=lambda uuid: env)
        out, kernel, initrd = run(ro)
        check("%s: a disk not to be written: next_entry booted, and said to stay" % name,
              "next_entry, 1>2" in out and "next_entry could not be cleared" in out and "-- read-only" in out, out)

        # A submenu's entry, named as GRUB names one: the submenu, '>', the
        # entry -- here each by its id.
        saved = make_disk(tmp, name + "-saved", layout, fs, rng, saved=True,
                          env=lambda uuid: [("saved_entry", "gnulinux-advanced-%s>gnulinux-6.8.0-1-advanced-%s"
                                             % (uuid, uuid))])
        out, kernel, initrd = run(saved)
        f = saved.files
        check("%s: GRUB_DEFAULT=saved -- grubenv's saved_entry, a submenu's entry by their ids" % name,
              "saved_entry, gnulinux-advanced-" in out and kernel == loaded(f[old_k]) and initrd == joined(f, old_i),
              out)
        bare = make_disk(tmp, name + "-bare", layout, fs, rng, saved=True,
                         env=lambda uuid: [("saved_entry", "gnulinux-6.8.0-1-advanced-%s" % uuid)])
        out, kernel, initrd = run(bare)
        check("%s: a saved_entry that names no entry where it looks: the first entry, said" % name,
              '"Ubuntu", the first entry, no entry being saved_entry' in out
              and kernel == loaded(bare.files[new_k]), out)
        disks += [nxt, saved]
    return disks


# What GRUB 2.06 does with each case below was found by booting it --
# grub-mkrescue's i386-pc GRUB 2.06-2ubuntu7.2 under QEMU, each `default`
# choosing an entry that says which it is, and the kernel printing the
# command line GRUB gave it. hvboot is held to the same answers.

# The `linux` line, and the command line the kernel was given.
COMMAND_LINES = [
    # An argument with a space in it in double quotes; ", ' and \ escaped.
    ("", r'''linux /vmlinuz console=ttyS0 root=/dev/x "a b c" 'q"uote' 'back\slash' "it's" plain''',
     r'''BOOT_IMAGE=/vmlinuz console=ttyS0 root=/dev/x "a b c" q\"uote back\\slash it\'s plain'''),
    # A tab is no space to it; "" is an argument, empty.
    ("", 'linux /vmlinuz console=ttyS0 "tab\tin" x\\ y "" end',
     'BOOT_IMAGE=/vmlinuz console=ttyS0 tab\tin "x y"  end'),
    ("", r'''linux /vmlinuz console=ttyS0 'say "hi" there' 'k=v w' "it's a\\b" end''',
     r'''BOOT_IMAGE=/vmlinuz console=ttyS0 "say \"hi\" there" "k=v w" "it\'s a\\b" end'''),
    # A variable's value outside quotes split at blanks, inside them whole;
    # one with no value no argument, "$empty" an empty one; `set` its first.
    ('set opts="foo bar"\nset lead=" lead"\nset trail="trail "\nset empty=\nset x2=$opts\n',
     'linux /vmlinuz console=ttyS0 $opts "$opts" x$opts y$lead z${trail}w $empty "$empty" "x2=$x2" end',
     'BOOT_IMAGE=/vmlinuz console=ttyS0 foo bar "foo bar" xfoo bar ylead ztrailw  x2=foo end'),
]
# An entry's body is run at boot, after the whole script: a variable set
# after the entry is what its body sees.
LATE = ("set opts=early\nmenuentry 'q9' {\n  linux /vmlinuz console=ttyS0 $opts\n}\nset opts=late\n",
        "BOOT_IMAGE=/vmlinuz console=ttyS0 late")

# A menu, and what each `default` booted.
MENU = "".join("menuentry 'E%d' {\n  linux /k%d\n}\n" % (i, i) for i in range(9)) + """menuentry 'A>B' {
  linux /kab
}
menuentry 'E10' {
  linux /k10
}
submenu 'S' {
  submenu 'T' {
    menuentry 'U' {
      linux /ku
    }
  }
  menuentry 'V' {
    linux /kv
  }
}
"""
DEFAULTS = [("0x2", "E2"), ("010", "E8"), ("08", "E8"), ("2nd", "E2"), (" 1", "E1"), ("A>>B", "A>B"),
            ("S>1", "V"), ("S>0>0", "U"), ("S>T>U", "U"), ("99", "E0"), ("11>1", "V"), ("A>B", "E0"), ("S", "U")]

# `search`'s forms, in an entry: the root each leaves (None: root as it was).
SEARCHES = [
    ("search -f /f", None),
    ("search --set -f /f", 'File("/f")'),
    ("search --set=v -f /f", None),
    ("search --set v -f /f", None),
    ("search -s v -f /f", None),
    ("search -f -s v /f", None),
    # -s takes the name for the variable, and with no name left GRUB looks
    # for that and sets root.
    ("search -f -s /f", 'File("/f")'),
    ("search /f", None),
    ("search.file /f", None),
    ("search.file /f v", None),
    ("search.file /f root somehint", 'File("/f")'),
    ("search --set=root --file /f extra-arg", 'File("/f")'),
    ("search --no-floppy --fs-uuid --set=root --hint-bios=hd0,gpt1 --hint-efi=hd0,gpt1 U1", 'FsUuid("U1")'),
    ("search -l -s root L1", 'Label("L1")'),
]


def reading(tmp):
    """grub.cfg read as GRUB 2.06 reads it, by `hvboot-test grub`."""
    def grub(name, text, *flags, env=None):
        path = os.path.join(tmp, name + ".cfg")
        with open(path, "w") as f:
            f.write(text)
        argv = ["grub", path] + list(flags)
        if env is not None:
            with open(path + ".env", "wb") as f:
                f.write(env)
            argv += ["--env", path + ".env"]
        return harness(*argv)

    for i, (pre, line, want) in enumerate(COMMAND_LINES):
        out, rc = grub("line%d" % i, pre + "menuentry 'q' {\n  %s\n}\n" % line)
        check("grub.cfg: `%s` -- the command line GRUB gives" % line.split(" ", 2)[-1][:40],
              rc == 0 and "command line: %s\n" % want in out, "wanted %r\n%s" % (want, out))
    out, rc = grub("late", LATE[0])
    check("grub.cfg: an entry's body read with its variables as the whole file leaves them, as GRUB runs it",
          rc == 0 and "command line: %s\n" % LATE[1] in out, out)
    for d, title in DEFAULTS:
        out, rc = grub("default", 'set default="%s"\n' % d + MENU)
        check('grub.cfg: default "%s" boots %s, as GRUB boots it' % (d, title),
              rc == 0 and 'chosen: "%s"' % title in out, out)

    out, rc = grub("braces", "menuentry 'First'{\n  linux /k1\n}\nmenuentry 'Second' {\n  linux /k2\n}\nset default=1\n")
    check("grub.cfg: a brace glued to a word is a token of its own -- 'First'{ opens its entry, the rest read",
          rc == 0 and 'menu: 0 "First" id None kernel Some("/k1")' in out and 'chosen: "Second"' in out, out)
    out, rc = grub("escaped", "menuentry 'X' {\n  echo \\}\n  linux /kx\n}\nmenuentry 'Y' {\n  linux /ky\n}\n"
                   "set default=Y\n")
    check("grub.cfg: an escaped brace is a word, not the end of its entry",
          rc == 0 and 'menu: 0 "X" id None kernel Some("/kx")' in out and 'chosen: "Y"' in out, out)
    out, rc = grub("assign", "default=1\nkopts=\"a b\"\nmenuentry 'first' {\n  linux /a\n}\nmenuentry 'second' {\n"
                   "  linux /b $kopts\n}\n")
    check("grub.cfg: assignments without `set` -- default=1, and a variable", rc == 0
          and 'chosen: "second"' in out and "command line: BOOT_IMAGE=/b a b\n" in out, out)
    out, rc = grub("unload", "menuentry 'p' {\n  if [ \"${initrdfail}\" = 1 ]; then\n    linux /k1 root=LABEL=r\n"
                   "    initrd /i1\n  else\n    linux /k2 root=PARTUUID=x panic=-1\n  fi\n}\n")
    check("grub.cfg: every branch read, a `linux` unloads the initrd before it -- Ubuntu's PARTUUID entry",
          rc == 0 and 'kernel: Some("/k2")' in out and "initrds: []" in out, out)
    out, rc = grub("options", "menuentry --class os 'Titled' --id tid {\n  linux /t\n}\n"
                   "menuentry 'Other' {\n  linux /o\n}\nset default=tid\n")
    check("grub.cfg: menuentry's options before its title", rc == 0 and 'chosen: "Titled"' in out, out)
    for i, (form, root) in enumerate(SEARCHES):
        text = "menuentry 'e' {\n  %s\n  linux /k\n}\n" % form
        out, rc = grub("search%d" % i, text)
        roots = [l for l in out.splitlines() if l.startswith("root: ")]
        check("grub.cfg: `%s` -- %s" % (form, "root %s" % root if root else "root not set"),
              rc == 0 and roots == (["root: %s" % root] if root else []), out)
    out, rc = grub("roots", "search --set=root --fs-uuid TOP\nsubmenu 'S' {\n  search --set=root --label SUB\n"
                   "  set default=0\n  menuentry 'e' {\n    linux /k\n  }\n}\nmenuentry 'f' {\n  linux /f\n}\n"
                   "set default=1\n")
    check("grub.cfg: a root outside every entry, a submenu's own its entries', a submenu's default not the menu's",
          rc == 0 and 'root outside: FsUuid("TOP")' in out and 'chosen: "f"' in out, out)
    out, rc = grub("roots2", "search --set=root --fs-uuid TOP\nsubmenu 'S' {\n  search --set=root --label SUB\n"
                   "  menuentry 'e' {\n    linux /k\n  }\n}\n")
    check("grub.cfg: and the submenu's entry starts from the submenu's root",
          rc == 0 and 'chosen: "e"' in out and 'root: Label("SUB")' in out, out)
    out, rc = grub("elsewhere", "configfile /boot/grub/other.cfg\nblscfg\nmenuentry 'm' {\n  linux /m $nosuch\n}\n")
    check("grub.cfg: another file read for the menu, and a variable with no value, said",
          rc == 0 and "note: `configfile`" in out and "note: `blscfg`" in out and "unknown: nosuch" in out, out)

    # grubenv as GRUB reads it: `\X` is X -- `\n` the letter, a backslash
    # and a newline a newline -- and nothing from a block with no signature.
    env = ENV_SIGNATURE + b"# WARNING: Do not edit this file by tools other than grub-editenv!!!\n" \
        + b"x=a\\\nb\ny=a\\\\nb\na=x\\ny\nnext_entry=1\n"
    env += b"#" * (1024 - len(env))
    cfg = 'if [ "${next_entry}" ] ; then\n  set default="${next_entry}"\nfi\nmenuentry \'a\' {\n  linux /a\n}\n' \
          "menuentry 'b' {\n  linux /b\n}\n"
    out, rc = grub("env", cfg, env=env)
    check("grubenv: values as GRUB decodes them, the warning a comment, next_entry taken",
          rc == 0 and 'env: x="a\\nb"' in out and 'env: y="a\\\\nb"' in out and 'env: a="xny"' in out
          and 'chosen: "b", grubenv\'s next_entry, 1, once' in out, out)
    out, rc = grub("noenv", cfg, env=b"next_entry=1\n")
    check("grubenv: one with no signature is not read, as GRUB's load_env refuses it",
          rc == 0 and "env: not an environment block" in out and 'chosen: "a"' in out, out)


def inode_of(fs_image, path):
    """Where `path`'s inode is in a filesystem image: its block and offset,
    from debugfs's "located at block N, offset 0xM"."""
    r = subprocess.run(["debugfs", "-R", "imap %s" % path, fs_image], capture_output=True, text=True)
    words = r.stdout.replace(",", " ").split()
    at = words.index("located")
    return int(words[at + 3]), int(words[at + 5], 16)


def refusals(tmp, rng, limits):
    """What hvboot says when it cannot boot, or passes something over."""
    kernel = bzimage(rng, 1 << 20)
    cfg = "menuentry 'L' {\n  linux /boot/vmlinuz root=/dev/vda1\n}\n"
    tree = {"/boot/vmlinuz": kernel, "/boot/grub/grub.cfg": cfg}

    disk, _, _ = fs_disk(tmp, "metabg", [tree], rng, extra=["-O", "meta_bg,^resize_inode"], table=False)
    out, rc = harness("image", disk)
    refusal = "vda: an ext filesystem with incompatible features 0x10 (meta_bg) this does not read"
    check("a filesystem refused for a feature is said by name, and listed when nothing is found",
          rc != 0 and refusal + ": passed over" in out
          and "cannot boot: no ext2/3/4 filesystem on the guest's disks -- " + refusal in out, out)

    disk, _, parts = fs_disk(tmp, "pastend", [tree, tree], rng)
    total = os.path.getsize(disk) // SECTOR
    (f1, _), (f2, n2) = parts
    write_gpt(disk, total, [(f1, total, LINUX_ROOT), (f2, n2, LINUX_ROOT)], rng)
    out, rc = harness("image", disk)
    check("a partition past the disk's end is said, and passed over", rc == 0
          and "vda1: sectors %d to %d, past the disk's end at %d: passed over" % (f1, f1 + total - 1, total) in out
          and "vda2/boot/grub/grub.cfg" in out, out)

    disk, _, _ = fs_disk(tmp, "noenv", [dict(tree, **{"/boot/grub/grubenv": b"next_entry=0\n" + b"#" * 1011})],
                         rng, table=False)
    out, rc = harness("image", disk)
    check("a grubenv GRUB would not read is said", rc == 0
          and "grubenv does not begin as GRUB's environment block does" in out, out)
    out, rc = harness("image", disk, "--cmdline", "hvboot.disk=7")
    check("hvboot.disk= past the guest's disks is said as that",
          rc != 0 and "cannot boot: hvboot.disk=7: the guest has 1 disk" in out, out)

    many = "menuentry 'many' {\n  linux /boot/vmlinuz\n  initrd" + " /boot/i" * 65 + "\n}\n"
    disk, _, _ = fs_disk(tmp, "manyinitrds", [dict(tree, **{"/boot/grub/grub.cfg": many, "/boot/i": b"x" * 100})],
                         rng, table=False)
    out, rc = harness("image", disk)
    check("an entry loading more initrds than hvboot holds open is refused by that",
          rc != 0 and "cannot boot: entry \"many\" names 65 initrds, more than the 64 this loads" in out, out)

    big = "menuentry 'L' {\n  linux /boot/vmlinuz\n}\n"
    big += "#" * (limits["CONFIG_MAX"] + 1 - len(big) - 1) + "\n"
    disk, _, _ = fs_disk(tmp, "toolong", [dict(tree, **{"/boot/grub/grub.cfg": big})], rng, table=False)
    out, rc = harness("image", disk)
    check("a grub.cfg longer than hvboot reads is refused by its name",
          rc != 0 and "cannot boot: vda/boot/grub/grub.cfg: a file of %d bytes, more than the %d this reads"
          % (limits["CONFIG_MAX"] + 1, limits["CONFIG_MAX"]) in out, out)

    # Two filesystems with a grub.cfg each, the first's /boot/grub broken
    # (its extent tree's magic gone): the search ends there, said, rather
    # than the second's being booted.
    first = dict(tree, **{"/boot/grub/grub.cfg": cfg.replace("'L'", "'on-1'")})
    second = dict(tree, **{"/boot/grub/grub.cfg": cfg.replace("'L'", "'on-2'")})
    disk, _, parts = fs_disk(tmp, "unreadable", [first, second], rng)
    (f1, n1) = parts[0]
    image = os.path.join(tmp, "unreadable.fs1")
    with open(disk, "rb") as f:
        f.seek(f1 * SECTOR)
        data = f.read(n1 * SECTOR)
    with open(image, "wb") as f:
        f.write(data)
    block, offset = inode_of(image, "/boot/grub")
    with open(disk, "r+b") as f:
        f.seek(f1 * SECTOR + block * 4096 + offset + 0x28)
        f.write(b"\0\0")
    out, rc = harness("image", disk)
    check("a grub.cfg that could not be looked for ends the search, said -- another's is not booted",
          rc != 0 and "cannot boot: vda1: GRUB's configuration could not be looked for: /boot/grub/grub.cfg" in out
          and "on-2" not in out, out)


def roots_outside(tmp, rng):
    """A root set outside every entry -- `search --set=root` above the menu
    -- is where the entries' files are: here on another filesystem than
    grub.cfg."""
    kernel, initrd = bzimage(rng, 1 << 20), rng.randbytes(5000)
    u1, u2 = new_uuid(rng), new_uuid(rng)
    cfg = ("search --no-floppy --fs-uuid --set=root %s\nmenuentry 'away' {\n  linux /vmlinuz-away root=/dev/vda2\n"
           "  initrd /initrd-away\n}\n" % u2)
    disk, _, _ = fs_disk(tmp, "outside", [{"/boot/grub/grub.cfg": cfg}, {"/vmlinuz-away": kernel,
                                                                        "/initrd-away": initrd}], rng, uuids=[u1, u2])
    d = Disk("outside", disk, {})
    out, k, i = run(d)
    check("a root set outside the entries: their kernel and initrd read off the filesystem it names",
          k == loaded(kernel) and i == initrd and "kernel: vda2/vmlinuz-away," in out, out)


def heap(tmp, rng, limits):
    """A grub.cfg as long as hvboot reads, of each shape that costs its
    parse the most a byte -- each booted inside hvboot's heap, all of it
    given back -- and an initrd whose extent tree is two levels deep, read
    whole there: a walk down two levels took a block of the heap a run and
    kept it, under the heap that gave back only its newest block."""
    n = limits["CONFIG_MAX"]
    kernel = bzimage(rng, 1 << 20)
    head = "menuentry 'first' {\n  linux /boot/vmlinuz root=/dev/vda\n}\n"

    def sized(prefix, unit, suffix):
        """`prefix`, `unit` as many times as fit, `suffix`: n bytes, the
        rest a comment."""
        text = prefix + unit * ((n - len(prefix) - len(suffix) - 1) // len(unit)) + suffix
        return text + "#" * (n - len(text) - 1) + "\n"

    def lines(make):
        """`head`, then line after line of `make(i)` while they fit: n bytes."""
        text, i = head, 0
        while len(text) + len(make(i)) < n:
            text += make(i)
            i += 1
        return text + "#" * (n - len(text) - 1) + "\n"

    mkconfig = "".join(grub_cfg(new_uuid(rng), "/boot", False) for _ in range(n // 6000 + 1))
    mkconfig = mkconfig[:mkconfig.rindex("\n", 0, n - len(head) - 1) + 1]
    shapes = {
        "one-letter words in an entry's linux line": sized(head + "menuentry 'w' {\n  linux /k", " a", "\n}\n"),
        "one-letter commands": sized(head, "a\n", ""),
        "assignments, each a variable of its own": lines(lambda i: "set v%d=x\n" % i),
        "bare assignments, each a variable of its own": lines(lambda i: "a%d=\n" % i),
        "`set`s with nothing to set": sized(head, "set\n", ""),
        "empty entries": sized(head, "menuentry e{}\n", ""),
        "entries with no body": sized(head, "menuentry e\n", ""),
        "empty submenus": sized(head, "submenu e{}\n", ""),
        # One word of variables and text, one after the other, read where it
        # is as each kind of word: an argument, a value, a title, a name.
        "a word of variables and text in a linux line": sized(head + "menuentry 'p' {\n  linux /k ", "-$a", "\n}\n"),
        "a word of variables and text as a value": sized(head + "set v=", "-$a", "\n"),
        "a word of variables and text as a title": sized(head + "menuentry ", "-$a", " {\n}\n"),
        "a word of variables and text as a command": sized(head, "-$a", "\n"),
        # A value made of another's, over and over: what variables add is
        # text the file does not have.
        "a variable's value used over and over": sized(head + "set a=" + "b" * 30000 + "\nset c=", "$a", "\n"),
        "quoted one-letter words": sized(head + "menuentry 'q' {\n  linux /k", ' "a"', "\n}\n"),
        "searches in an entry": sized(head + "menuentry 's' {\n", "  search --set=root -u x\n", "}\n"),
        "one-letter initrds": sized(head + "menuentry 'i' {\n  initrd", " a", "\n}\n"),
        # Each of a submenu's entries starts from the roots its body sets:
        # thousands of both.
        "a submenu's roots and its entries": sized(head + "submenu 's' {\n" + "search.file x root\n" * (n // 40),
                                                   "menuentry e{}\n", "}\n"),
        "nesting past the parser's depth": sized(head, "if a; then\n", ""),
        "grub-mkconfig's, a kernel after another": sized(head + mkconfig, "#", ""),
    }
    # And beside each, grubenv as long as hvboot reads one, of variables as
    # small as they come: what its reading costs the most.
    env = ENV_SIGNATURE
    while len(env) + len(b"v%d=\n" % len(env)) < limits["ENV_MAX"]:
        env += b"v%d=\n" % len(env)
    env += b"#" * (limits["ENV_MAX"] - len(env))
    peaks = []
    for i, (what, text) in enumerate(shapes.items()):
        disk, _, _ = fs_disk(tmp, "shape%d" % i, [{"/boot/vmlinuz": kernel, "/boot/grub/grub.cfg": text,
                                                   "/boot/grub/grubenv": env}], rng, table=False)
        out, rc = harness("image", disk)
        peak = [l for l in out.splitlines() if l.startswith("heap: ")]
        peaks.append("%s %s" % (what, peak[-1][6:] if peak else "?"))
        check("a grub.cfg of %d bytes of %s, booted inside hvboot's heap" % (len(text.encode()), what),
              rc == 0 and len(text.encode()) == n and ': "first", ' in out, out)
    print("  heap: " + "; ".join(peaks))

    # The initrd written into the holes every other of 2900 one-block files
    # left: 1450 runs of a block, a tree two levels deep.
    initrd = rng.randbytes(1450 * 4096)
    cfg = "menuentry 'deep' {\n  linux /boot/vmlinuz\n  initrd /boot/initrd-deep\n}\n"
    disk, _, _ = fs_disk(tmp, "deep", [{"/boot/vmlinuz": kernel, "/boot/grub/grub.cfg": cfg}], rng, table=False)
    one, whole = os.path.join(tmp, "one-block"), os.path.join(tmp, "initrd-deep")
    with open(one, "wb") as f:
        f.write(rng.randbytes(4096))
    with open(whole, "wb") as f:
        f.write(initrd)
    cmds = os.path.join(tmp, "deep.cmds")
    with open(cmds, "w") as f:
        f.write("".join("write %s f%d\n" % (one, i) for i in range(2900)))
        f.write("".join("rm f%d\n" % i for i in range(1, 2900, 2)))
        f.write("write %s /boot/initrd-deep\n" % whole)
    subprocess.run(["debugfs", "-w", "-f", cmds, disk], capture_output=True, check=True)
    r = subprocess.run(["debugfs", "-R", "ex /boot/initrd-deep", disk], capture_output=True, text=True)
    deep = any(l.split()[:1] == ["2/"] or l.strip().startswith("2/ 2") for l in r.stdout.splitlines())
    check("an initrd whose extent tree is two levels deep, as debugfs lays it out", deep, r.stdout[:1000])
    out, k, i = run(Disk("deep", disk, {}))
    check("read whole, byte for byte, inside hvboot's heap", k == loaded(kernel) and i == initrd, out)

    out, rc = harness("arena", "--seed", "1", "--iterations", "300000")
    check("hvboot's heap's arithmetic held to a model of it, given back whole", rc == 0, out)


def gate(args, tmp):
    rng = random.Random(1)
    out, _ = harness("limits")
    limits = {w[0]: int(w[1]) for w in (l.split() for l in out.splitlines()) if len(w) == 2 and w[0].isupper()}
    disks = layouts(args, tmp, rng)
    reading(tmp)
    refusals(tmp, rng, limits)
    roots_outside(tmp, rng)
    heap(tmp, rng, limits)
    for d in disks:
        r = subprocess.run([BIN, "fuzz", d.path, "--seed", str(args.seed), "--iterations", str(args.iterations)],
                           capture_output=True, text=True, timeout=3600)
        check("%s: %d disks of its metadata changed at random, no panic, no read outside it, every run ending, "
              "inside hvboot's heap and all of it given back" % (d.name, args.iterations), r.returncode == 0,
              r.stdout + r.stderr)


def image(args):
    """A distribution's image: what hvboot would boot off it, and the image
    fuzzed."""
    for path in args.image:
        r = subprocess.run([BIN, "image", path], capture_output=True, text=True)
        print(r.stdout.strip())
        check("%s: hvboot finds a grub.cfg and an entry to boot" % os.path.basename(path), r.returncode == 0,
              r.stdout + r.stderr)
        r = subprocess.run([BIN, "fuzz", path, "--seed", str(args.seed), "--iterations", str(args.iterations)],
                           capture_output=True, text=True, timeout=7200)
        print(r.stdout.strip())
        check("%s: its metadata changed at random, %d times: no finding" % (os.path.basename(path),
              args.iterations), r.returncode == 0, r.stdout + r.stderr)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--image", action="append", default=[], help="a disk image made raw, to report on and fuzz")
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--iterations", type=int, default=2000, help="fuzzed disks per disk")
    ap.add_argument("--keep", action="store_true", help="keep the disks made")
    args = ap.parse_args()

    tools = ["cargo"] + ([] if args.image else ["mke2fs", "e2fsck", "debugfs"])
    for t in tools:
        if shutil.which(t) is None:
            sys.exit("hvboot-test needs %s" % t)
    r = subprocess.run(["cargo", "build", "--release", "--offline", "--quiet", "--target-dir", OUT], cwd=FUZZ)
    if r.returncode != 0:
        sys.exit("hvboot-test: fuzz/hvboot did not build")

    if args.image:
        image(args)
    else:
        tmp = tempfile.mkdtemp(prefix="nos-hvboot-")
        try:
            gate(args, tmp)
        finally:
            if args.keep or failures:
                print("disks in " + tmp)
            else:
                shutil.rmtree(tmp, ignore_errors=True)
    print()
    if failures:
        print("FAILED: " + ", ".join(failures))
        return 1
    print("hvboot-test: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
