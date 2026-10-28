#!/usr/bin/env python3
"""ext4 test: filesystems e2fsprogs made, mounted by nos's ext4 driver and
read from its shell -- held to what the host put on them.

The fuzzer holds the driver to the fuzzer's own images (fs-fuzz.py's ext4
targets). This holds it to mke2fs's, as Linux's distributions make them --
ext4 by default, metadata_csum, 64bit, flex_bg, extents, 256-byte inodes --
holding files small and large, a sparse file of a thousand pieces whose
extent tree has leaves of its own, a directory of hundreds of entries that
e2fsck -D indexed, a name too long for the kernel's tree, a symlink:

  - mounted read-only, every file read by the kernel's `crc32` must have
    the CRC-32 of the bytes the host wrote, every directory and file `find`
    lists must be one the host made, and the long name and the symlink not
    among them;
  - mounted for writing, names changed from the shell -- directories made,
    one grown past a block, files made, renamed and moved, a directory
    moved, the indexed directory given an entry (which drops its index),
    files removed, a sparse one of a thousand extents among them, and a
    directory tree -- `find` and `crc32` must show what the changes make,
    and again after it is unmounted and mounted read-only; after the boot
    `e2fsck -fn` must find nothing at all in the image;
  - an image whose journal debugfs left unreplayed, a transaction in it
    rewriting a block of a file: the driver must read the file as the
    replay would leave it, and write nothing -- the image's bytes are the
    same after the boot as before it;
  - then nos powers off, and nothing panicked on the way;
  - a second boot changes names on a fresh copy of mke2fs's image and is
    killed, never unmounted: e2fsck must replay its journal and find
    nothing after, and Linux (a loop mount, under sudo or in the nos-builder
    image --privileged) must replay it too, show the tree the changes make
    and leave nothing for e2fsck.

    scripts/ext4-test.py [--tcg] [--keep]

arm64, because it drives the shell over UDP and that is the boot whose
command line carries one; the driver is the same code on either
architecture. mke2fs and debugfs are the host's, or the nos-builder
image's when the host has none. Exit code 0 = every check passed.
"""

import argparse
import hashlib
import importlib.util
import os
import platform
import random
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

spec = importlib.util.spec_from_file_location("pt", os.path.join(HERE, "parttest.py"))
pt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pt)

SHELL_PORT = pt.SHELL_PORT
EXT4_MIB = 96
JOURNAL_MIB = 80


def e2(tmp, *args):
    """An e2fsprogs tool run over files in `tmp` (written as `{}` in the
    arguments): the host's, or the nos-builder image's with `tmp` at /img.
    Its exit code and what it said."""
    if shutil.which(args[0]):
        argv = [a.replace("{}", tmp) for a in args]
    else:
        argv = ["docker", "run", "--platform", "linux/amd64", "--rm", "-v", "%s:/img" % tmp, "-w", "/img",
                "nos-builder"] + [a.replace("{}", "/img") for a in args]
    r = subprocess.run(argv, capture_output=True, text=True)
    return r.returncode, (r.stdout + r.stderr).strip()


def content(tmp, rng):
    """The tree to put on the images: what the host wrote of each file."""
    d = os.path.join(tmp, "content")
    files = {}

    def put(path, data):
        full = os.path.join(d, path)
        os.makedirs(os.path.dirname(full), exist_ok=True)
        with open(full, "wb") as f:
            f.write(data)
        files[path] = data

    put("small.txt", b"a small file\n")
    put("medium.bin", rng.randbytes(100 << 10))
    put("large.bin", rng.randbytes(5 << 20))
    sparse = bytearray(4 * 4096 * 1000)
    for i in range(1000):
        sparse[i * 4 * 4096:i * 4 * 4096 + 4096] = rng.randbytes(4096)
    put("sparse.bin", bytes(sparse))
    for i in range(300):
        put("big/entry-with-a-longish-name-%04d" % i, b"%d\n" % i)
    put("a/b/c/deep.txt", b"deep\n")
    put("empty", b"")
    # What the kernel's tree leaves out: a name of 70 bytes, a symlink.
    with open(os.path.join(d, "n" * 70), "wb") as f:
        f.write(b"hidden by its name\n")
    os.symlink("small.txt", os.path.join(d, "a-link"))
    return files


def make(tmp, name, mib):
    img = os.path.join(tmp, name)
    with open(img, "wb") as f:
        f.truncate(mib << 20)
    mkfs = ["mke2fs", "-q", "-F", "-t", "ext4", "-b", "4096", "-L", "ext4test", "-d"]
    if shutil.which("mke2fs"):
        code, out = e2(tmp, *(mkfs + ["{}/content", "{}/" + name]))
    else:
        # A macOS bind mount serves no extended attributes, which -d lists:
        # the tree is copied into the container first, as mkrootfs.sh does.
        r = subprocess.run(["docker", "run", "--platform", "linux/amd64", "--rm", "-v", "%s:/img" % tmp,
                            "nos-builder", "bash", "-c",
                            "cp -a /img/content /tmp/content && " + " ".join(mkfs + ["/tmp/content", "/img/" + name])],
                           capture_output=True, text=True)
        code, out = r.returncode, (r.stdout + r.stderr).strip()
    if not pt.check("%s: mke2fs -t ext4" % name, code == 0, out):
        return None
    code, out = e2(tmp, "e2fsck", "-fyD", "{}/" + name)
    pt.check("%s: e2fsck -D indexes its directories" % name, code in (0, 1), out)
    return img


def journaled(tmp, files, rng):
    """The image with a transaction in its journal nobody replayed: the
    first block of medium.bin written anew. What the file then reads as."""
    img = make(tmp, "journal.img", JOURNAL_MIB)
    if img is None:
        return None, None
    code, out = e2(tmp, "debugfs", "-R", "bmap /medium.bin 0", "{}/journal.img")
    blk = re.search(r"^\s*(\d+)\s*$", out, re.M)
    if not pt.check("journal.img: medium.bin's first block found", code == 0 and blk is not None, out):
        return None, None
    newer = rng.randbytes(4096)
    with open(os.path.join(tmp, "newer"), "wb") as f:
        f.write(newer)
    with open(os.path.join(tmp, "cmds"), "w") as f:
        f.write("jo -c\njw -b %s {}/newer\njc\n".replace("{}", "/img" if not shutil.which("debugfs") else tmp)
                % blk.group(1))
    code, out = e2(tmp, "debugfs", "-w", "-f", "{}/cmds", "{}/journal.img")
    _, stats = e2(tmp, "dumpe2fs", "-h", "{}/journal.img")
    pt.check("journal.img: debugfs leaves a transaction unreplayed", code == 0 and "needs_recovery" in stats,
             out + stats[:400])
    return img, newer + files["medium.bin"][4096:]


def crc_of(sh, path):
    out = sh.run("crc32 %s" % path)
    m = re.search(r"crc32 0x([0-9A-Fa-f]+), (\d+) bytes", out)
    return (int(m.group(1), 16), int(m.group(2))) if m else None


def listing(sh, top):
    """The directories and files `find` lists under `top`: (dirs, files),
    paths below it, lost+found left out."""
    dirs, files = set(), set()
    for kind, into in (("d", dirs), ("f", files)):
        for line in sh.run("find %s -type %s" % (top, kind)).splitlines():
            line = line.strip()
            if line.startswith(top + "/"):
                path = line[len(top) + 1:]
                if path != "lost+found" and not path.startswith("lost+found/"):
                    into.add(path)
    return dirs, files


def dirs_of(files):
    """Every directory the paths in `files` are under."""
    out = set()
    for f in files:
        parts = f.split("/")[:-1]
        for k in range(1, len(parts) + 1):
            out.add("/".join(parts[:k]))
    return out


class Model:
    """The tree the shell's changes make: directories, and files with their
    bytes."""

    def __init__(self, files):
        self.files = dict(files)
        self.dirs = dirs_of(files)

    def mkdir(self, sh, top, path):
        sh.run("mkdir %s/%s" % (top, path))
        self.dirs.add(path)

    def touch(self, sh, top, path):
        sh.run("touch %s/%s" % (top, path))
        self.files[path] = b""

    def mv(self, sh, top, old, new):
        sh.run("mv %s/%s %s/%s" % (top, old, top, new))

        def moved(p):
            return new + p[len(old):] if p == old or p.startswith(old + "/") else p
        self.files = {moved(p): d for p, d in self.files.items()}
        self.dirs = {moved(p) for p in self.dirs}

    def rm(self, sh, top, path):
        sh.run("rm %s/%s" % (top, path))

        def gone(p):
            return p == path or p.startswith(path + "/")
        self.files = {p: d for p, d in self.files.items() if not gone(p)}
        self.dirs = {p for p in self.dirs if not gone(p)}


def change(sh, top, m, grow):
    """Names changed under `top`, as the model has them -- what both boots
    do: a directory made and grown by `grow` entries past its first block,
    files made and moved, a directory moved, the indexed directory given an
    entry, files with blocks removed, a directory tree removed."""
    m.mkdir(sh, top, "made")
    m.mkdir(sh, top, "made/sub")
    m.touch(sh, top, "made/file")
    for i in range(grow):
        m.touch(sh, top, "made/entry-with-a-longish-name-%04d" % i)
    m.touch(sh, top, "big/added")
    m.mv(sh, top, "small.txt", "made/moved.txt")
    m.mv(sh, top, "a", "made/a")
    m.mv(sh, top, "made/sub", "made/sub2")
    m.mkdir(sh, top, "made/sub2/deeper")
    m.mv(sh, top, "made/sub2/deeper", "deeper-moved")
    m.rm(sh, top, "big/entry-with-a-longish-name-0007")
    m.rm(sh, top, "sparse.bin")
    m.rm(sh, top, "made/a")
    m.rm(sh, top, "empty")


def held_to(sh, top, m, what):
    """`find` and `crc32` under `top` held to the model's tree."""
    dirs, files = listing(sh, top)
    pt.check("%s: find lists the directories the changes make" % what, dirs == m.dirs,
             "extra %s, missing %s" % (sorted(dirs - m.dirs)[:5], sorted(m.dirs - dirs)[:5]))
    pt.check("%s: find lists the files the changes make" % what, files == set(m.files),
             "extra %s, missing %s" % (sorted(files - set(m.files))[:5], sorted(set(m.files) - files)[:5]))
    wrong = []
    for path, data in sorted(m.files.items()):
        if data:
            got = crc_of(sh, "%s/%s" % (top, path))
            if got != (zlib.crc32(data), len(data)):
                wrong.append("%s: %s, made %08X of %d" % (path, got, zlib.crc32(data), len(data)))
    pt.check("%s: every file with bytes in it reads as it should" % what, not wrong, "\n".join(wrong[:10]))


# What a crash leaves that is no damage: the superblock's free counts,
# which ext4 keeps lazily -- Linux's too -- and every mount puts right.
LAZY_COUNTS = re.compile(r"^Free (blocks|inodes) count wrong \(")


def fsck_clean(tmp, name, what, crashed=False):
    """e2fsck -fn's verdict on image `name`: nothing found at all -- or,
    for one a crash left, nothing but the superblock's free counts."""
    code, out = e2(tmp, "e2fsck", "-fn", "{}/" + name)
    lines = out.splitlines()
    asked = [lines[k - 1] if k else "" for k, line in enumerate(lines) if line.rstrip().endswith("? no")]
    damage = [q for q in asked if not (crashed and LAZY_COUNTS.match(q))]
    return pt.check("%s: e2fsck -fn finds nothing%s" % (what, " but the lazy free counts" if crashed else ""),
                    code == 0 and not damage, out[-2000:])


LINUX_MOUNT = r"""set -e
mkdir -p /tmp/m
mount -o loop "$IMG" /tmp/m
(cd /tmp/m && find . -mindepth 1 \( -type d -printf 'd %P
' \) -o \( -type f -printf 'f %P
' \)) > "$OUT"
umount /tmp/m
e2fsck -fn "$IMG"
"""


def linux_mount(tmp, name):
    """Image `name` mounted by Linux -- a loop mount, which replays its
    journal -- listed and unmounted, then checked by e2fsck: (ok, the
    listing, what was said). Under sudo on Linux, in the nos-builder image
    elsewhere; None where neither can be had."""
    if platform.system() == "Linux":
        if subprocess.run(["sudo", "-n", "true"], capture_output=True).returncode != 0:
            return None
        argv = ["sudo", "env", "IMG=%s" % os.path.join(tmp, name), "OUT=%s" % os.path.join(tmp, "linux.txt"),
                "bash", "-c", LINUX_MOUNT]
    elif shutil.which("docker"):
        argv = ["docker", "run", "--platform", "linux/amd64", "--rm", "--privileged", "-v", "%s:/img" % tmp,
                "-e", "IMG=/img/" + name, "-e", "OUT=/img/linux.txt", "nos-builder", "bash", "-c", LINUX_MOUNT]
    else:
        return None
    r = subprocess.run(argv, capture_output=True, text=True)
    path = os.path.join(tmp, "linux.txt")
    listed = open(path).read() if os.path.exists(path) else ""
    return r.returncode == 0, listed, (r.stdout + r.stderr)[-2000:]


def disk_named(sh, mib):
    out = sh.run("disks")
    for line in out.splitlines():
        m = re.match(r"\s*(\S+)\s+\d+ sectors \((\d+) MB\)", line)
        if m and int(m.group(2)) == mib:
            return m.group(1)
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tcg", action="store_true")
    ap.add_argument("--keep", action="store_true", help="leave the images behind")
    args = ap.parse_args()

    rng = random.Random(1)
    tmp = tempfile.mkdtemp(prefix="nos-ext4test-")
    root = os.path.join(tmp, "root.img")
    log = os.path.join(tmp, "serial.log")
    pt.mkrootfs(root)
    files = content(tmp, rng)
    ext4 = make(tmp, "ext4.img", EXT4_MIB)
    jimg, j_medium = journaled(tmp, files, rng)
    if ext4 is None or jimg is None:
        return 1
    with open(jimg, "rb") as f:
        before = hashlib.sha256(f.read()).hexdigest()

    # The second boot's: mke2fs's image as it was made.
    kimg = os.path.join(tmp, "kill.img")
    shutil.copyfile(ext4, kimg)

    hvf = platform.system() == "Darwin" and platform.machine() == "arm64" and not args.tcg
    accel = ["-accel", "hvf", "-cpu", "host"] if hvf else ["-accel", "tcg", "-cpu", "cortex-a72"]

    def qemu(disks, serial):
        argv = ["qemu-system-aarch64", "-M", "virt,gic-version=3", "-smp", "4", "-m", "1024"] + accel + [
            "-kernel", os.path.join(ROOT, "nos-arm64.img"),
            "-append", "root=auto udpshell=%d" % SHELL_PORT,
            "-global", "virtio-mmio.force-legacy=false"]
        for k, d in enumerate([root] + disks):
            argv += ["-drive", "file=%s,format=raw,id=hd%d,if=none" % (d, k),
                     "-device", "virtio-blk-device,drive=hd%d" % k]
        argv += ["-device", "virtio-net-device,netdev=n0",
                 "-netdev", "user,id=n0,hostfwd=udp::%d-:%d" % (SHELL_PORT, SHELL_PORT),
                 "-serial", "file:" + serial, "-display", "none"]
        return subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    model = Model(files)
    p = qemu([ext4, jimg], log)
    try:
        if not pt.check("boots to its shell", pt.wait_log(log, "boot: complete", 300)):
            return 1
        sh = pt.Shell()
        e4, j4 = disk_named(sh, EXT4_MIB), disk_named(sh, JOURNAL_MIB)
        if not pt.check("the two ext4 disks found", e4 is not None and j4 is not None, sh.run("disks")):
            return 1
        out = sh.run("mount ext4 %s /e4 ro" % e4)
        pt.check("mke2fs's ext4 mounted, read-only", "mounted ext4 on /e4 (ro)" in out, out)

        wrong = []
        for path, data in sorted(files.items()):
            got = crc_of(sh, "/e4/" + path)
            if got != (zlib.crc32(data), len(data)):
                wrong.append("%s: %s, made %08X of %d" % (path, got, zlib.crc32(data), len(data)))
        pt.check("every one of its %d files reads as the host wrote it" % len(files), not wrong,
                 "\n".join(wrong[:10]))

        listed = sh.run("find /e4 -type f")
        found = {l.strip()[len("/e4/"):] for l in listed.splitlines() if l.strip().startswith("/e4/")}
        found.discard("lost+found")
        pt.check("find lists the files the host made, and none it left out of the tree",
                 found == set(files), "extra %s, missing %s" % (sorted(found - set(files))[:5],
                                                                sorted(set(files) - found)[:5]))

        out = sh.run("umount /e4")
        pt.check("unmounted", "unmounted /e4" in out, out)
        out = sh.run("mount ext4 %s /e4" % e4)
        pt.check("mke2fs's ext4 mounted for writing", "mounted ext4 on /e4" in out and "(ro)" not in out, out)
        change(sh, "/e4", model, 150)
        held_to(sh, "/e4", model, "names changed")
        out = sh.run("mounts")
        pt.check("mounts shows its journal", re.search(r"journal=\d+/\d+ seq=\d+", out) is not None, out)
        out = sh.run("umount /e4")
        pt.check("unmounted after writing", "unmounted /e4" in out, out)
        out = sh.run("mount ext4 %s /e4 ro" % e4)
        pt.check("mounted again, read-only", "mounted ext4 on /e4 (ro)" in out, out)
        held_to(sh, "/e4", model, "mounted again")

        out = sh.run("mount ext4 %s /j4 ro" % j4)
        pt.check("the image with a journal nobody replayed mounted, read-only",
                 "mounted ext4 on /j4 (ro)" in out, out)
        got = crc_of(sh, "/j4/medium.bin")
        pt.check("its file read as the journal's transaction leaves it",
                 got == (zlib.crc32(j_medium), len(j_medium)), "%s, want %08X" % (got, zlib.crc32(j_medium)))
        out = sh.run("mounts")
        pt.check("mounts says the journal was read through, not replayed", "journal-not-replayed=" in out, out)

        sh.sock.settimeout(5)
        try:
            sh.run("poweroff")
        except Exception:
            pass
        for _ in range(60):
            if p.poll() is not None:
                break
            time.sleep(0.5)
        pt.check("the machine switches itself off", p.poll() is not None)
    finally:
        pt.kill(p)

    with open(jimg, "rb") as f:
        after = hashlib.sha256(f.read()).hexdigest()
    pt.check("nothing was written to the journaled image", before == after)
    text = open(log, errors="replace").read()
    pt.check("nothing panicked, the way down included", "PANIC" not in text, text[-2000:])
    fsck_clean(tmp, "ext4.img", "the image the driver wrote and unmounted")

    # The second boot: names changed, and the machine killed under them.
    klog = os.path.join(tmp, "kill.log")
    killed = Model(files)
    p = qemu([kimg], klog)
    try:
        if pt.check("the second boot boots to its shell", pt.wait_log(klog, "boot: complete", 300)):
            sh = pt.Shell()
            k4 = disk_named(sh, EXT4_MIB)
            out = sh.run("mount ext4 %s /k4" % k4)
            if pt.check("mke2fs's ext4 mounted for writing, again",
                        "mounted ext4 on /k4" in out and "(ro)" not in out, out):
                change(sh, "/k4", killed, 40)
                held_to(sh, "/k4", killed, "before the kill")
    finally:
        if p.poll() is None:
            p.kill()
            p.wait()
    ktext = open(klog, errors="replace").read()
    pt.check("the second boot: nothing panicked before the kill", "PANIC" not in ktext, ktext[-2000:])
    shutil.copyfile(kimg, os.path.join(tmp, "kill-linux.img"))
    # The replay alone: with -f, e2fsck would go on to repair what -fn is to find.
    code, out = e2(tmp, "e2fsck", "-y", "-E", "journal_only", "{}/kill.img")
    pt.check("the killed image: e2fsck replays its journal", code in (0, 1) and "recovering journal" in out,
             out[-2000:])
    fsck_clean(tmp, "kill.img", "the killed image, replayed", crashed=True)
    linux = linux_mount(tmp, "kill-linux.img")
    if linux is None:
        print("SKIP: Linux's loop mount of the killed image (neither sudo nor docker)")
    else:
        ok, listed, said = linux
        pt.check("the killed image: Linux mounts it, replaying its journal, and e2fsck finds nothing after",
                 ok, said)
        dirs = {line[2:] for line in listed.splitlines() if line.startswith("d ")}
        dirs = {d for d in dirs if d != "lost+found" and not d.startswith("lost+found/")}
        seen = {line[2:] for line in listed.splitlines() if line.startswith("f ")}
        want = set(killed.files) | {"n" * 70}
        pt.check("the killed image: Linux shows the tree the changes make", dirs == killed.dirs and seen == want,
                 "dirs extra %s, missing %s; files extra %s, missing %s"
                 % (sorted(dirs - killed.dirs)[:5], sorted(killed.dirs - dirs)[:5], sorted(seen - want)[:5],
                    sorted(want - seen)[:5]))
    if args.keep or pt.failures:
        print("images in " + tmp)
    else:
        shutil.rmtree(tmp, ignore_errors=True)
    print()
    if pt.failures:
        print("FAILED: " + ", ".join(pt.failures))
        return 1
    print("ext4-test: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
