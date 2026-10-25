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

e2fsck is the host's, or the one in the nos-builder image when the host has
none, as mkrootfs does. Exit code 0 = every check passed and the image is
clean.

The way down is judged as well: the unmount writes the superblock's state,
and after it the machine has to switch itself off with nothing panicking.
Until 7908bf6 it could not -- an exiting CPU left its one WFI, and the static
destructors faulted after the unmount -- and this test looked away from it.

`find` walks a tree made for it there, and is held to what each test
should pick out of it: by name and by path, by type, size and emptiness,
by depth, with `!`, `-o` and parentheses; with what it walked in order,
procfs entered at /proc unless `-xdev` keeps it out, and what it refuses
said.

And a file past 4 GiB: the image starts without large_file, which nos has
to set when it makes its first file of 2 GiB or more; one is grown by
`truncate` and written at its end, past 5 GiB, where the size takes both of
an inode's words and the block hangs under the triple indirect block -- the
whole of it does, past 64 MiB of 1 KiB blocks; another is grown past 6 GiB
and cut back; and the self-test's far file is run on the root. Then e2fsck
judges it all, and debugfs reads the far file's size, and where its last
line is, off the image -- another implementation of the format, which a
driver reading back its own writes is not.
"""

import argparse
import importlib.util
import os
import re
import shutil
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

# The far file: grown by truncate, a line appended at its end. 5 GiB is past
# 2 and 4 GiB, and inside the 16 GiB a file of 1 KiB blocks may be.
FAR_SIZE = 5 << 30
FAR_TEXT = "far-past-five-gib"
# The one grown and cut back.
CUT_FROM = 6 << 30
CUT_TO = 1 << 20


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

    find_work(sh)
    far_work(sh)

    sh.run("sync")


def find_paths(sh, args):
    """What `find` prints: the paths, in order, and the lines that say
    something went wrong."""
    out = sh.run("find " + args)
    lines = [l for l in out.splitlines() if l.strip()]
    return [l for l in lines if not l.startswith("find: ")], [l for l in lines if l.startswith("find: ")], out


def find_work(sh):
    """`find` over a tree made for it: what each expression picks out."""
    t = "/work/find"
    for cmd in ["mkdir %s", "write %s/a.txt x", "write %s/B.TXT yy", "touch %s/empty.txt", "mkdir %s/sub",
                "write %s/sub/c.log some-log-line", "mkdir %s/sub/deep", "write %s/sub/deep/d.txt deep-down",
                "mkdir %s/void"]:
        sh.run(cmd % t)

    def under(*names):
        return {t + n for n in names}

    every = under("", "/a.txt", "/B.TXT", "/empty.txt", "/sub", "/sub/c.log", "/sub/deep", "/sub/deep/d.txt", "/void")
    paths, errors, out = find_paths(sh, t)
    pt.check("find walks the whole tree, itself included", set(paths) == every and len(paths) == len(every)
             and not errors, out)
    pt.check("and prints a directory before what is in it",
             all(paths.index(p.rsplit("/", 1)[0]) < paths.index(p) for p in paths if p != t), out)

    cases = [
        ("-name *.txt", under("/a.txt", "/empty.txt", "/sub/deep/d.txt"), "by name"),
        ("-iname *.txt", under("/a.txt", "/B.TXT", "/empty.txt", "/sub/deep/d.txt"), "by name, whatever the case"),
        ("-name [ab]*", under("/a.txt"), "by a class"),
        ("-path */sub/* ! -type d", under("/sub/c.log", "/sub/deep/d.txt"), "by path, and not"),
        ("-type d", under("", "/sub", "/sub/deep", "/void"), "by type"),
        ("-empty", under("/empty.txt", "/void"), "what is empty"),
        ("-type f -size +1", under("/B.TXT", "/sub/c.log", "/sub/deep/d.txt"), "files over a byte"),
        ("-size -2", under("/a.txt", "/empty.txt"), "files under two bytes, and no directory"),
        ("-size 13c", under("/sub/c.log"), "a file of thirteen bytes"),
        ("-mindepth 1 -maxdepth 1", under("/a.txt", "/B.TXT", "/empty.txt", "/sub", "/void"), "one level down"),
        ("-maxdepth 0", under(""), "the start alone"),
        ("( -name a* -o -name c* ) -type f", under("/a.txt", "/sub/c.log"), "with -o in parentheses"),
        ("-not -name *.* -a -mindepth 1", under("/sub", "/sub/deep", "/void"), "with -not and -a"),
    ]
    for args, want, what in cases:
        paths, errors, out = find_paths(sh, "%s %s" % (t, args))
        pt.check("find %s: %s" % (args, what), set(paths) == want and len(paths) == len(want) and not errors, out)

    # Across a mount: into procfs at /proc, unless -xdev keeps it out
    paths, errors, out = find_paths(sh, "/ -path /proc/*")
    pt.check("find goes into the procfs mounted at /proc",
             {"/proc/version", "/proc/cmdline", "/proc/interrupts"} <= set(paths) and not errors, out)
    paths, errors, out = find_paths(sh, "/ -xdev -path /proc*")
    pt.check("and -xdev keeps it to the root's filesystem", paths == ["/proc"] and not errors, out)

    # What it cannot do, it says
    out = sh.run("find /work/nothing -name x")
    pt.check("find says a path is not there", "find: /work/nothing: not found" in out, out)
    out = sh.run("find %s -bogus" % t)
    pt.check("find refuses a test it does not know", "unknown test '-bogus'" in out and t not in out, out)
    out = sh.run("find %s ( -name a*" % t)
    pt.check("find refuses a '(' left open", "not closed" in out and t not in out, out)


def far_work(sh):
    """Files past 4 GiB, from the shell: their sizes as stat has them, and
    the self-test's far file on the root."""
    out = sh.run("truncate /work/far.bin 5G")
    pt.check("truncate grows a file past 4 GiB", "/work/far.bin: %d bytes" % FAR_SIZE in out, out)
    sh.run("append /work/far.bin " + FAR_TEXT)
    out = sh.run("stat /work/far.bin")
    pt.check("a line appended at its end lands past 5 GiB",
             "%d bytes" % (FAR_SIZE + len(FAR_TEXT)) in out, out)

    sh.run("truncate /work/cut.bin 6G")
    sh.run("append /work/cut.bin gone-again")
    out = sh.run("truncate /work/cut.bin 1M")
    pt.check("a file past 6 GiB cut back to 1 MiB", "/work/cut.bin: %d bytes" % CUT_TO in out, out)

    out = sh.run("fstest /work")
    pt.check("the self-test's far file passes on the root",
             "a sparse file past 4 GiB" in out and "fstest: passed" in out, out[-600:])


def far_checks(image):
    """What debugfs reads of the far files off the image nos left: large_file
    set, the far file's size, its last line where nos wrote it, and the one
    cut back holding no block at all."""
    _, stats = e2fs(image, "debugfs", "-R", "stats")
    pt.check("nos set large_file", re.search(r"(?m)^Filesystem features:.*\blarge_file\b", stats) is not None,
             "\n".join(l for l in stats.splitlines() if "features" in l))
    bs = int(re.search(r"(?m)^Block size:\s+(\d+)", stats).group(1))

    _, st = e2fs(image, "debugfs", "-R", "stat /work/far.bin")
    m = re.search(r"Group:\s+\d+\s+Size:\s+(\d+)", st)
    pt.check("debugfs reads the far file's size, past 5 GiB",
             m is not None and int(m.group(1)) == FAR_SIZE + len(FAR_TEXT), st[:600])
    pt.check("and finds its last block under the triple indirect block", "(TIND)" in st, st[:600])

    _, at = e2fs(image, "debugfs", "-R", "bmap /work/far.bin %d" % (FAR_SIZE // bs))
    phys = [int(l) for l in at.splitlines() if l.strip().isdigit()]
    text = b""
    if phys and phys[-1] != 0:
        with open(image, "rb") as f:
            f.seek(phys[-1] * bs + FAR_SIZE % bs)
            text = f.read(len(FAR_TEXT))
    pt.check("and the line appended at 5 GiB where nos put it", text == FAR_TEXT.encode(), "%r, %r" % (at, text))

    _, st = e2fs(image, "debugfs", "-R", "stat /work/cut.bin")
    m = re.search(r"Group:\s+\d+\s+Size:\s+(\d+)", st)
    pt.check("the file cut back is 1 MiB, and holds no block",
             m is not None and int(m.group(1)) == CUT_TO and re.search(r"Blockcount:\s+0\b", st) is not None,
             st[:600])


def wait_for(log, marker, timeout):
    """Like parttest's wait_log, but a panic does not end the test here: the
    image is still worth an e2fsck, and the log is judged at the end."""
    start = time.time()
    while time.time() - start < timeout:
        if os.path.exists(log) and marker in open(log, errors="replace").read():
            return True
        time.sleep(0.5)
    return False


def e2fs(image, tool, *args):
    """A tool of e2fsprogs run on the image: the host's, or the one in the
    builder image when the host has none -- the way mkrootfs.sh picks its
    mke2fs. Its exit code, and what it said."""
    if shutil.which(tool):
        argv = [tool] + list(args) + [image]
    else:
        at = os.path.dirname(image)
        name = os.path.basename(image)
        argv = ["docker", "run", "--platform", "linux/amd64", "--rm",
                "-v", "%s:/img" % at, "-w", "/img", "nos-builder",
                tool] + list(args) + ["/img/" + name]
    run = subprocess.run(argv, capture_output=True, text=True)
    return run.returncode, (run.stdout + run.stderr).strip()


def fsck(image):
    """What e2fsck makes of the image."""
    return e2fs(image, "e2fsck", "-fn")


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
    # Without large_file, which mke2fs gives every image: nos's first file of
    # 2 GiB or more has to set it
    e2fs(image, "debugfs", "-w", "-R", "feature -large_file")
    _, stats = e2fs(image, "debugfs", "-R", "stats")
    if not pt.check("the image starts without large_file",
                    re.search(r"(?m)^Filesystem features:", stats) is not None
                    and re.search(r"(?m)^Filesystem features:.*\blarge_file\b", stats) is None, stats[:800]):
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
        # and then the machine has to switch itself off.
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
        pt.check("and the machine switches itself off", p.poll() is not None)
    finally:
        if p.poll() is None:
            p.send_signal(signal.SIGTERM)
            try:
                p.wait(10)
            except subprocess.TimeoutExpired:
                p.kill()

    code, out = fsck(image)
    pt.check("e2fsck finds the image clean after all of that", code == 0, out)
    far_checks(image)

    # Nothing must go wrong from the boot to the power-off, the halt after
    # the unmount included: its static destructors faulted on arm64 until
    # 7908bf6, and this used to stop reading at "Stopping cpu".
    text = open(log, errors="replace").read()
    pt.check("nothing panicked, the way down included",
             "PANIC" not in text,
             "\n".join(l for l in text.splitlines() if "PANIC" in l))

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
