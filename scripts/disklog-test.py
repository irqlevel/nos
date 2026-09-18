#!/usr/bin/env python3
"""Disk log test: boot with a prepared area, then read it back off the image.

The disk log writes the kernel log to a raw disk area as each line is
produced, for the machine that has no serial port, no working NIC and so no
netconsole, and stops somewhere in boot without saying anything. Nothing else
in the suite touches it: a smoke boot never gives `disklog=on`, and without
that parameter no disk is so much as read.

What is checked is both halves of the channel. The kernel's: it finds the
area, claims the device -- so a mount and a format of that disk are refused
-- and writes whole sectors with no failures. And the host tool's:
`scripts/disklog.py read` parses the header this boot left and hands back the
text, which has to be this boot's log from its first line to its last. A
driver reading back its own writes would agree with itself whatever it put on
the disk; the host tool is the second implementation that would not.

    scripts/disklog-test.py [--tcg] [--keep]

arm64, because it drives the shell over UDP and that is the boot whose
command line carries one -- `disklog` itself is the same code on either
architecture.

Exit code 0 = every check passed.
"""

import argparse
import importlib.util
import os
import platform
import re
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

# The area, and the disk it sits on. 64 MiB is a great many boots' worth of
# log and what the host tool lays down by default.
AREA_BYTES = 64 * 1024 * 1024
DISK_MB = 128

# How many lines of the boot are compared with the serial console's, line for
# line. They are all from before the writer task exists, so the disk holds
# every one of them and the two channels have to agree exactly.
HEAD_LINES = 50


def disklog(*args):
    return subprocess.run([sys.executable, os.path.join(HERE, "disklog.py"), *args],
                          capture_output=True, text=True)


def shell_checks(sh):
    """What the kernel says about the area it found, and what the claim on it
    keeps out."""
    out = sh.run("disklog")
    first = out.strip().splitlines()[0] if out.strip() else ""

    if not pt.check("the disk log found a prepared area",
                    "no prepared area" not in out and "disklog: off" not in out, out):
        return None

    pt.check("on this boot, the first since the area was prepared",
             "boot 1," in first, first)
    pt.check("and wrote whole sectors to it, none of them failing",
             " failures 0," in out and "sector writes 0\n" not in out, out)

    # Which disk it took: the name in the report, checked against `disks`
    names = [line.split()[0] for line in sh.run("disks").splitlines() if line.strip()]
    named = re.match(r"disklog: (\S+?),", first)
    disk = named.group(1) if named else ""
    if not pt.check("the report names a disk the kernel has", disk in names, first):
        return None

    # The claim is the point: nothing else may write where the log goes.
    refused = sh.run("format nanofs %s" % disk)
    pt.check("nothing else may format the disk it claimed",
             "in use by the disk log" in refused, refused)
    refused = sh.run("diskwrite %s 4 55" % disk)
    pt.check("nor write to it raw",
             "in use by the disk log" in refused, refused)
    refused = sh.run("mount ext2 %s /x" % disk)
    pt.check("nor mount it", "failed" in refused.lower(), refused)
    return disk


def image_checks(area, serial):
    """What is on the disk, read by the host tool's own idea of the layout,
    and judged against the same boot's serial console -- the check a driver
    reading back its own writes cannot make, because it would agree with
    itself whatever it had put there."""
    read = disklog("read", area)
    if not pt.check("the host tool reads the area back", read.returncode == 0,
                    read.stderr.strip()):
        return

    pt.check("the header says this boot wrote it", "boot seq 1," in read.stderr,
             read.stderr.strip())
    pt.check("and that it recorded something", " 0 bytes recorded" not in read.stderr,
             read.stderr.strip())

    on_disk = read.stdout.splitlines()
    traced = [line for line in open(serial, errors="replace").read().splitlines()
              if line.startswith("0:")]
    if not pt.check("the boot traced something to compare with", len(traced) > HEAD_LINES):
        return

    # The whole boot, from its first line. Everything traced before the area
    # was found was held in memory and written the moment it was, so the disk
    # starts where the console does -- the part of a boot that dies early
    # that no other channel can carry.
    pt.check("the disk holds the boot's very first traced line",
             on_disk[:1] == traced[:1], "\n  disk:   %s\n  serial: %s"
             % (on_disk[0] if on_disk else "(nothing)", traced[0]))
    pt.check("and the %d after it, line for line" % HEAD_LINES,
             on_disk[:HEAD_LINES] == traced[:HEAD_LINES],
             next((("disk %r != serial %r" % (a, b))
                   for a, b in zip(on_disk[:HEAD_LINES], traced[:HEAD_LINES]) if a != b),
                  "the disk has %d of %d lines" % (len(on_disk), HEAD_LINES)))

    text = read.stdout
    pt.check("the log on disk reaches the end of the boot", "boot: complete" in text,
             text[-300:] if text else "(nothing)")
    # Stop() runs after the shell is gone: what it pushed out is the proof
    # that the last lines make it to the disk rather than dying in the ring.
    pt.check("and the lines traced on the way down", "Shutdown requested" in text,
             text[-300:])
    pt.check("no panic reached it", "PANIC" not in text, text[-600:])


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tcg", action="store_true")
    ap.add_argument("--keep", action="store_true", help="leave the images behind")
    args = ap.parse_args()

    kernel = os.path.join(ROOT, "nos-arm64.img")
    if not os.path.exists(kernel):
        sys.exit("%s not found -- build with: make nocheck ARCH=aarch64" % kernel)

    tmp = tempfile.mkdtemp(prefix="nos-disklog-")
    root = os.path.join(tmp, "root.img")
    area = os.path.join(tmp, "area.img")
    log = os.path.join(tmp, "serial.log")
    pt.mkrootfs(root)
    with open(area, "wb") as f:
        f.truncate(DISK_MB * 1024 * 1024)

    prepared = disklog("format", area, "-b", str(AREA_BYTES))
    if not pt.check("an area is prepared on a blank disk", prepared.returncode == 0,
                    prepared.stderr.strip()):
        return 1
    print("  " + prepared.stdout.strip())

    hvf = platform.system() == "Darwin" and platform.machine() == "arm64" and not args.tcg
    accel = ["-accel", "hvf", "-cpu", "host"] if hvf else ["-accel", "tcg", "-cpu", "cortex-a72"]
    argv = ["qemu-system-aarch64", "-M", "virt,gic-version=3", "-smp", "4", "-m", "1024"] + accel + [
        "-kernel", kernel,
        "-append", "root=auto disklog=on udpshell=%d" % SHELL_PORT,
        "-global", "virtio-mmio.force-legacy=false",
        "-drive", "file=%s,format=raw,id=hd0,if=none" % root,
        "-device", "virtio-blk-device,drive=hd0",
        "-drive", "file=%s,format=raw,id=hd1,if=none" % area,
        "-device", "virtio-blk-device,drive=hd1",
        "-device", "virtio-net-device,netdev=n0",
        "-netdev", "user,id=n0,hostfwd=udp::%d-:%d" % (SHELL_PORT, SHELL_PORT),
        "-serial", "file:" + log, "-display", "none"]

    p = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        if not pt.check("reaches the shell", pt.wait_log(log, "boot: complete", 300)):
            return 1

        sh = pt.Shell()
        shell_checks(sh)

        # the log is pushed out and the claim given back on the way down
        sh.sock.settimeout(5)
        try:
            sh.run("poweroff")
        except Exception:
            pass
        for _ in range(40):
            if p.poll() is not None:
                break
            time.sleep(0.5)
    finally:
        pt.kill(p)

    image_checks(area, log)

    if args.keep:
        print("images left in " + tmp)
    ok = not pt.failures
    print("disklog-test: " + ("PASSED" if ok else "FAILED: " + ", ".join(pt.failures)))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
