#!/usr/bin/env python3
"""ext4 test: filesystems e2fsprogs made, mounted by nos's ext4 driver and
read from its shell -- held to what the host put on them.

The fuzzer holds the driver to the fuzzer's own images (fs-fuzz.py's ext4
targets). This holds it to mke2fs's, as Linux's distributions make them --
ext4 by default, metadata_csum, 64bit, flex_bg, extents, 256-byte inodes --
holding files small and large, a sparse file of a thousand pieces whose
extent tree has leaves of its own, a directory of hundreds of entries that
e2fsck -D indexed, a name too long for the kernel's tree, a symlink:

  - mounted read-only -- all the driver does yet -- every file read by the
    kernel's `crc32` must have the CRC-32 of the bytes the host wrote, every
    directory and file `find` lists must be one the host made, and the
    long name and the symlink not among them;
  - an image whose journal debugfs left unreplayed, a transaction in it
    rewriting a block of a file: the driver must read the file as the
    replay would leave it, and write nothing -- the image's bytes are the
    same after the boot as before it;
  - then nos powers off, and nothing panicked on the way.

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

    hvf = platform.system() == "Darwin" and platform.machine() == "arm64" and not args.tcg
    accel = ["-accel", "hvf", "-cpu", "host"] if hvf else ["-accel", "tcg", "-cpu", "cortex-a72"]
    argv = ["qemu-system-aarch64", "-M", "virt,gic-version=3", "-smp", "4", "-m", "1024"] + accel + [
        "-kernel", os.path.join(ROOT, "nos-arm64.img"),
        "-append", "root=auto udpshell=%d" % SHELL_PORT,
        "-global", "virtio-mmio.force-legacy=false",
        "-drive", "file=%s,format=raw,id=hd0,if=none" % root, "-device", "virtio-blk-device,drive=hd0",
        "-drive", "file=%s,format=raw,id=hd1,if=none" % ext4, "-device", "virtio-blk-device,drive=hd1",
        "-drive", "file=%s,format=raw,id=hd2,if=none" % jimg, "-device", "virtio-blk-device,drive=hd2",
        "-device", "virtio-net-device,netdev=n0",
        "-netdev", "user,id=n0,hostfwd=udp::%d-:%d" % (SHELL_PORT, SHELL_PORT),
        "-serial", "file:" + log, "-display", "none"]
    p = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        if not pt.check("boots to its shell", pt.wait_log(log, "boot: complete", 300)):
            return 1
        sh = pt.Shell()
        e4, j4 = disk_named(sh, EXT4_MIB), disk_named(sh, JOURNAL_MIB)
        if not pt.check("the two ext4 disks found", e4 is not None and j4 is not None, sh.run("disks")):
            return 1
        out = sh.run("mount ext4 %s /e4" % e4)
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

        out = sh.run("mount ext4 %s /j4" % j4)
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
