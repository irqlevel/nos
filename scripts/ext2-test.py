#!/usr/bin/env python3
"""ext2 write test: work the root filesystem from the shell, then let e2fsck
judge what was left on the disk.

The boot-time self-test (`fstest=on`) checks that the driver reads back what
it wrote. This checks the other half: that the image is still one a
filesystem checker calls clean -- the bitmaps, the counts, the link counts
and the directory entries all agreeing -- after files have been made, grown,
truncated, renamed and removed.

    scripts/ext2-test.py [--tcg] [--keep]

arm64 only, because it drives the shell over UDP and that is the boot whose
command line carries one; the ext2 driver itself is the same code on either
architecture, and x86 exercises it through the smoke test's `fstest=on`.

e2fsck runs in the nos-builder image, as mkrootfs does when the host has no
e2fsprogs. Exit code 0 = every check passed and the image is clean.

What this does not judge: the shutdown path after the filesystems are
unmounted. arm64 has a known fault in the static destructors there
(TaskTable::~TaskTable), which happens long after the superblock is written.
"""

import argparse
import importlib.util
import os
import signal
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

spec = importlib.util.spec_from_file_location("pt", os.path.join(HERE, "parttest.py"))
pt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pt)

SHELL_PORT = pt.SHELL_PORT

# A file big enough to reach the indirect blocks of a 1 KiB-block image: 12
# direct blocks are 12 KiB, so 64 KiB is well past them, and past the first
# doubly-indirect boundary at 268 KiB it is not -- the boot-time fstest goes
# there, and this one goes over the same ground from the shell.
BIG_LINES = 400


def shell_work(sh):
    """Make, grow, move and remove things, the way a shell session would."""
    sh.run("mkdir /work")
    sh.run("mkdir /work/inner")
    sh.run("write /work/a.txt hello-from-the-shell")
    pt.check("a file written reads back",
             "hello-from-the-shell" in sh.run("cat /work/a.txt"))

    # Grow a file past its direct blocks, one append at a time
    line = "line-%04d-padding-padding-padding-padding"
    for i in range(BIG_LINES):
        sh.run("append /work/big.txt " + line % i)
    out = sh.run("stat /work/big.txt")
    want = BIG_LINES * len(line % 0)   # append writes the text, nothing more
    pt.check("the grown file is as long as what went into it",
             ("%d bytes" % want) in out, out)

    sh.run("cp /work/a.txt /work/inner/copy.txt")
    pt.check("the copy has the same content",
             "hello-from-the-shell" in sh.run("cat /work/inner/copy.txt"))

    sh.run("mv /work/a.txt /work/renamed.txt")
    pt.check("the rename moved it", "hello-from-the-shell" in sh.run("cat /work/renamed.txt"))
    pt.check("and left nothing behind", "not found" in sh.run("cat /work/a.txt").lower())

    sh.run("write /work/gone.txt to-be-removed")
    sh.run("rm /work/gone.txt")
    pt.check("a removed file is gone", "not found" in sh.run("cat /work/gone.txt").lower())

    # A directory with something in it, removed whole
    sh.run("mkdir /work/tree")
    sh.run("write /work/tree/leaf.txt leaf")
    sh.run("rm /work/tree")

    listing = sh.run("ls /work")
    pt.check("what is left is what should be",
             "big.txt" in listing and "renamed.txt" in listing
             and "inner" in listing and "tree" not in listing, listing)

    sh.run("sync")


def wait_for(log, marker, timeout):
    """Like parttest's wait_log, but a panic is not the end of the world
    here: the shutdown path panics on arm64 after everything is unmounted."""
    start = time.time()
    while time.time() - start < timeout:
        if os.path.exists(log) and marker in open(log, errors="replace").read():
            return True
        time.sleep(0.5)
    return False


def fsck(image):
    """What e2fsck makes of the image, through Docker as mkrootfs does."""
    at = os.path.dirname(image)
    name = os.path.basename(image)
    run = subprocess.run(
        ["docker", "run", "--platform", "linux/amd64", "--rm",
         "-v", "%s:/img" % at, "-w", "/img", "nos-builder",
         "e2fsck", "-fn", "/img/" + name],
        capture_output=True, text=True)
    return run.returncode, (run.stdout + run.stderr).strip()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tcg", action="store_true")
    ap.add_argument("--keep", action="store_true", help="leave the image behind")
    args = ap.parse_args()

    tmp = tempfile.mkdtemp(prefix="nos-ext2test-")
    image = os.path.join(tmp, "root.img")
    log = os.path.join(tmp, "serial.log")
    pt.mkrootfs(image)

    code, out = fsck(image)
    if not pt.check("the fresh image is clean to start with", code == 0, out):
        return 1

    import platform
    hvf = platform.system() == "Darwin" and platform.machine() == "arm64" and not args.tcg
    accel = ["-accel", "hvf", "-cpu", "host"] if hvf else ["-accel", "tcg", "-cpu", "cortex-a72"]
    argv = ["qemu-system-aarch64", "-M", "virt,gic-version=3", "-smp", "4", "-m", "1024"] + accel + [
        "-kernel", os.path.join(ROOT, "nos-arm64.img"),
        "-append", "root=auto udpshell=%d" % SHELL_PORT,
        "-global", "virtio-mmio.force-legacy=false",
        "-drive", "file=%s,format=raw,id=hd0,if=none" % image,
        "-device", "virtio-blk-device,drive=hd0",
        "-device", "virtio-net-device,netdev=n0",
        "-netdev", "user,id=n0,hostfwd=udp::%d-:%d" % (SHELL_PORT, SHELL_PORT),
        "-serial", "file:" + log, "-display", "none"]

    p = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        if not pt.check("boots with the image as its root",
                        pt.wait_log(log, "MountRootFs: mounted ext2 on /", 300)):
            return 1
        if not pt.check("reaches the shell", pt.wait_log(log, "boot: complete", 300)):
            return 1

        sh = pt.Shell()
        shell_work(sh)

        # Down cleanly: the unmount is what writes the superblock's state,
        # and it happens before the shutdown path this does not judge.
        sh.sock.settimeout(5)
        try:
            sh.run("poweroff")
        except Exception:
            pass

        pt.check("unmounts the root on the way down", wait_for(log, "unmounting /", 60))
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

    code, out = fsck(image)
    pt.check("e2fsck finds the image clean after all of that", code == 0, out)

    # Nothing must go wrong while the filesystem is being used. The
    # shutdown that follows is another matter: arm64 has a known fault in
    # the static destructors after everything is unmounted (the panic names
    # TaskTable::~TaskTable), and this test is not the place to judge it.
    text = open(log, errors="replace").read()
    up = text.split("Stopping cpu")[0]
    pt.check("nothing panicked while the filesystem was in use",
             "PANIC" not in up,
             "\n".join(l for l in up.splitlines() if "PANIC" in l))

    if args.keep:
        print("image left at " + image)
    else:
        subprocess.run(["rm", "-rf", tmp])

    print()
    if pt.failures:
        print("FAILED: " + ", ".join(pt.failures))
        return 1
    print("ext2-test: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
