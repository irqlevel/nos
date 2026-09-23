#!/usr/bin/env python3
"""hv Linux test: load a real Linux bzImage into a guest under the nos
hypervisor and watch its early serial console come out.

This is the third of the four demos in plans/03-hypervisor.md -- a bzImage
printing its early console -- and it is a *manual* gate, like the hardware
NIC ones: it needs a bzImage, which is megabytes and which CI cannot build in
the time it has, so it is not in ci.yml and is pointed at a kernel by hand.

Build a small 64-bit guest kernel (a tinyconfig with 8250 serial and an early
console is enough; PCI, ACPI and SMP off), then:

    scripts/hv-linux-test.py --bzimage /path/to/bzImage
    scripts/hv-linux-test.py --bzimage bzImage --initrd initramfs.cpio.gz \\
        --cmdline "earlyprintk=serial,ttyS0,115200 console=ttyS0 nolapic no_timer_check"

What it checks: nos boots, the hv module loads and turns the extension on,
`hv boot` streams the guest's console to the kernel log (hvguest| ...), and
the guest gets far enough into early boot to print the kernel banner and set
up its memory -- under AMD-V, which on a machine with no hardware SVM is
QEMU's TCG (slow: the whole run is a minute or two).

Exit code 0 = every required marker appeared.
"""

import argparse
import importlib.util
import os
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

spec = importlib.util.spec_from_file_location("hvt", os.path.join(HERE, "hv-test.py"))
hvt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hvt)
pt = hvt.pt

# The lines the guest has to reach: the banner, and enough of early setup to
# show it is a real kernel running and not just the decompressor stub. Each
# is matched inside a "hvguest| " line, so nothing of nos's own console can
# satisfy one.
REQUIRED = [
    r"Linux version",
    r"Command line:",
    r"NX \(Execute Disable\) protection: active",
    r"console \[ttyS0\] enabled",
]
# A later line that shows the guest got well past the banner into device
# setup; not required, but reported when it appears.
NICE = r"Kernel command line:"

DEFAULT_CMDLINE = "earlyprintk=serial,ttyS0,115200 console=ttyS0 nolapic no_timer_check"


def rootfs(tmp, bzimage, initrd, boot_cmd):
    rootdir = os.path.join(tmp, "rootdir")
    os.makedirs(os.path.join(rootdir, "etc"), exist_ok=True)
    shutil.copy(hvt.module("x86_64"), rootdir)
    shutil.copy(bzimage, os.path.join(rootdir, "bzImage"))
    if initrd:
        shutil.copy(initrd, os.path.join(rootdir, "initrd"))
    rc = ["insmod /hv.ko", "hv on", boot_cmd]
    with open(os.path.join(rootdir, "etc", "rc"), "w") as f:
        f.write("# scripts/hv-linux-test.py\n" + "\n".join(rc) + "\n")

    image = os.path.join(tmp, "root.img")
    # A roomy filesystem: the kernel and initrd have to fit on it.
    subprocess.run([os.path.join(HERE, "mkrootfs.sh"), image, "128", rootdir],
                   cwd=ROOT, check=True, stdout=subprocess.DEVNULL)
    return image


def guest_lines(log):
    return [l for l in open(log, errors="replace").read().splitlines() if "hvguest|" in l]


def run(args):
    import re

    tmp = tempfile.mkdtemp(prefix="nos-hvlinux-")
    log = os.path.join(tmp, "serial.log")

    boot_cmd = "hv boot /bzImage mem=%d secs=%d" % (args.mem, args.secs)
    if args.initrd:
        boot_cmd += " initrd=/initrd"
    boot_cmd += " cmdline=" + args.cmdline
    image = rootfs(tmp, args.bzimage, args.initrd, boot_cmd)

    kvm = os.path.exists("/dev/kvm") and not args.tcg and hvt.host_has_svm()
    print("accelerator: %s" % ("KVM, the host's AMD-V" if kvm else "TCG, -cpu max"))
    argv = ["qemu-system-x86_64", "-display", "none", "-m", "2G", "-smp", "4",
            "-cpu", "host" if kvm else "max",
            "-cdrom", os.path.join(ROOT, "nos.iso"), "-serial", "file:" + log,
            "-drive", "file=%s,format=raw,id=drive0,if=none" % image,
            "-device", "virtio-blk-pci,drive=drive0,disable-legacy=on,disable-modern=off"]
    if kvm:
        argv += ["-enable-kvm"]

    p = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        if not pt.check("nos boots and reaches the boot command",
                        pt.wait_log(log, "> " + boot_cmd, args.deadline)):
            return
        # The guest's console comes back in the command's report block, which
        # is printed when the vCPU stops -- so wait for the block's end (or a
        # nos panic), then assert the guest's markers are in it. The vCPU runs
        # for its whole budget here, since without a timer interrupt it does
        # not reach a halt; the banner and device setup are all in the first
        # guest millisecond regardless.
        done = False
        t0 = time.time()
        while time.time() - t0 < args.deadline:
            txt = open(log, errors="replace").read()
            if "PANIC:" in txt:
                pt.check("nos did not panic", False, txt[-2000:])
                return
            if "--- end ttyS0 ---" in txt or "the guest printed nothing" in txt:
                done = True
                break
            time.sleep(3)

        if not pt.check("the guest ran and its console came back", done,
                        "\n".join(guest_lines(log)[-10:]) or "(no report block)"):
            return
        txt = open(log, errors="replace").read()
        block = txt[txt.find("--- ttyS0 ---"):txt.find("--- end ttyS0 ---")] if "--- ttyS0 ---" in txt else ""
        for m in REQUIRED:
            pt.check("the guest printed: %s" % m, re.search(m, block) is not None,
                     block[-1500:])
        if re.search(NICE, block):
            print("note: the guest reached %r" % NICE)
    finally:
        pt.kill(p)
        tail = guest_lines(log)
        if args.keep or not tail:
            print("log at " + log)
        if tail and args.verbose:
            print("\n".join(tail))
        if not args.keep:
            shutil.rmtree(tmp, ignore_errors=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bzimage", required=True, help="a 64-bit Linux bzImage")
    ap.add_argument("--initrd", help="an initramfs image, optional")
    ap.add_argument("--cmdline", default=DEFAULT_CMDLINE, help="the guest kernel command line")
    ap.add_argument("--mem", type=int, default=256, help="guest RAM in MiB")
    ap.add_argument("--secs", type=int, default=120, help="guest run budget in seconds")
    ap.add_argument("--deadline", type=int, default=360, help="seconds to wait for the markers")
    ap.add_argument("--tcg", action="store_true", help="do not use KVM even if the host has AMD-V")
    ap.add_argument("--keep", action="store_true", help="keep the serial log")
    ap.add_argument("--verbose", action="store_true", help="print the guest's console at the end")
    args = ap.parse_args()

    if not os.path.exists(args.bzimage):
        sys.exit("no such bzImage: " + args.bzimage)
    if not os.path.exists(os.path.join(ROOT, "nos.iso")):
        sys.exit("build nos.iso first (make)")

    run(args)

    print()
    if pt.failures:
        print("FAILED: " + ", ".join(pt.failures))
        return 1
    print("hv-linux-test: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
