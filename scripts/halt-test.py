#!/usr/bin/env python3
"""The halt, with a shell inside a module's command and another still asking.

`poweroff` unloads the modules and then stops the shells -- in that order,
because a shell may be inside a module's command that returns only when its
module is told to stop (an `hv boot` on the console as `poweroff` comes over
SSH), and the halt that waited for the shells first waited for that one for
ever, a join's report every ten seconds. A boot cannot see the difference by
itself: with nothing running in a module the two orders end the same. This
boot has something running:

  - `modtest hold` typed at the serial console -- a call that returns when
    modtest's stop has run, or by itself after ten seconds saying no stop
    came;
  - `poweroff` over the UDP shell while it holds. The hold has to be let go
    by the stop, at once: under the old order the console's shell was waited
    for first, and the hold ran its ten seconds out;
  - slowexit under modtest in the list, whose exit takes eight seconds: the
    halt is in the middle of the modules for that long, with the UDP shell
    still answering. An `insmod` then has to be refused -- a module loaded
    behind the halt's back would run on while the files and the soft IRQs
    went from under it -- and what the exits say has to be on the console,
    which the shell keeps the log off until the halt takes it back;
  - and then the machine has to switch itself off, with nothing panicked:
    modtest's drop panics if a call of its command is still running.

    scripts/halt-test.py [--tcg]                 arm64 (HVF on a Mac)
    scripts/halt-test.py --arch x86_64 [--iso nos-test.iso]

x86-64 needs an ISO whose command line has the UDP shell, which nos.iso's has
not: `make nos-test.iso TEST_ISO_ARGS=udpshell=9100`. Exit code 0 = every
check passed.
"""

import argparse
import importlib.util
import os
import platform
import shutil
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

# What modtest's hold says of how it ended (modules/test/modtest)
RELEASED = "modtest hold: released by the stop, its command and provider closed before it"
NEVER = "modtest hold: never released"
# How long the hold is given to begin before the machine is told to go, and
# how long the machine has to switch itself off: slowexit's eight seconds,
# the unmount, and an emulated CPU's own time
HOLD_SETTLE = 2.0
OFF_TIMEOUT = 120


def rootfs(tmp, arch):
    rootdir = os.path.join(tmp, "rootdir")
    os.makedirs(os.path.join(rootdir, "etc"))
    for m in ("slowexit", "modtest", "hello"):
        shutil.copy(pt.module_path(arch, m), rootdir)
    # slowexit first: the halt unloads the newest first, so modtest goes --
    # and lets the hold go -- before slowexit's exit takes its time
    with open(os.path.join(rootdir, "etc", "rc"), "w") as f:
        f.write("# halt-test.py\ninsmod /slowexit.ko\ninsmod /modtest.ko\n")
    image = os.path.join(tmp, "root.img")
    subprocess.run([os.path.join(HERE, "mkrootfs.sh"), image, "64", rootdir, "1024"], cwd=ROOT, check=True,
                   stdout=subprocess.DEVNULL)
    return image


def qemu(arch, tcg, image, iso):
    if arch == "aarch64":
        hvf = platform.system() == "Darwin" and platform.machine() == "arm64" and not tcg
        accel = ["-accel", "hvf", "-cpu", "host"] if hvf else ["-accel", "tcg", "-cpu", "cortex-a72"]
        return ["qemu-system-aarch64", "-M", "virt,gic-version=3", "-smp", "4", "-m", "1024"] + accel + [
            "-kernel", os.path.join(ROOT, "nos-arm64.img"),
            "-append", "root=auto udpshell=%d" % SHELL_PORT,
            "-global", "virtio-mmio.force-legacy=false",
            "-drive", "file=%s,format=raw,id=hd0,if=none" % image,
            "-device", "virtio-blk-device,drive=hd0",
            "-device", "virtio-net-device,netdev=n0",
            "-netdev", "user,id=n0,hostfwd=udp::%d-:%d" % (SHELL_PORT, SHELL_PORT),
            "-serial", "stdio", "-display", "none", "-no-reboot"]
    kvm = os.path.exists("/dev/kvm") and platform.machine() in ("x86_64", "AMD64") and not tcg
    cpu = ["-enable-kvm", "-cpu", "host"] if kvm else ["-cpu", "max"]
    return ["qemu-system-x86_64", "-display", "none", "-m", "1G", "-smp", "2"] + cpu + [
        "-cdrom", iso,
        "-drive", "file=%s,format=raw,id=drive0,if=none" % image,
        "-device", "virtio-blk-pci,drive=drive0,disable-legacy=on,disable-modern=off",
        "-device", "virtio-net-pci,netdev=n0,disable-legacy=on,disable-modern=off",
        "-netdev", "user,id=n0,hostfwd=udp::%d-:%d" % (SHELL_PORT, SHELL_PORT),
        "-serial", "stdio", "-no-reboot"]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--arch", choices=("aarch64", "x86_64"), default="aarch64")
    ap.add_argument("--tcg", action="store_true")
    ap.add_argument("--iso", default=os.path.join(ROOT, "nos-test.iso"),
                    help="x86-64: an ISO whose command line has udpshell=%d" % SHELL_PORT)
    ap.add_argument("--keep", action="store_true", help="leave the image and the log behind")
    args = ap.parse_args()

    if args.arch == "x86_64" and not os.path.exists(args.iso):
        sys.exit("no %s -- make nos-test.iso TEST_ISO_ARGS=udpshell=%d" % (args.iso, SHELL_PORT))

    tmp = tempfile.mkdtemp(prefix="nos-halttest-")
    image = rootfs(tmp, args.arch)
    log = os.path.join(tmp, "serial.log")

    # The serial console on a pipe: what is typed at it is the console
    # shell's, and what the kernel says there is the log this reads
    out = open(log, "wb")
    p = subprocess.Popen(qemu(args.arch, args.tcg, image, args.iso), cwd=ROOT, stdin=subprocess.PIPE, stdout=out,
                         stderr=subprocess.STDOUT)

    def text():
        return open(log, errors="replace").read()

    try:
        if not pt.check("boots", pt.wait_log(log, "boot: complete", 300)):
            return 1

        # /etc/rc is the console shell's to run, after "boot: complete":
        # both modules loaded, as the UDP shell sees them
        sh = pt.Shell(timeout=30)
        loaded = ""
        for _ in range(60):
            loaded = sh.run("lsmod")
            if "modtest" in loaded and "slowexit" in loaded:
                break
            time.sleep(0.5)
        if not pt.check("slowexit and modtest are loaded by /etc/rc", "modtest" in loaded and "slowexit" in loaded,
                        loaded):
            return 1

        # The console's shell into a call that only modtest's stop ends
        p.stdin.write(b"\nmodtest hold\n")
        p.stdin.flush()
        time.sleep(HOLD_SETTLE)
        pt.check("the hold is held: it has said nothing yet", "modtest hold:" not in text())

        told = time.time()
        sh.sock.settimeout(10)
        try:
            sh.run("poweroff")
        except Exception:
            pass

        # The stop is the first thing the halt does to modtest: the hold is
        # let go well inside its own ten seconds
        let_go = False
        while time.time() - told < 6:
            if RELEASED in text():
                let_go = True
                break
            time.sleep(0.2)
        pt.check("the console's `modtest hold` is let go by the halt's stop of modtest, the shell not waited for "
                 "first", let_go and NEVER not in text(), text()[-1500:])

        # slowexit's exit is taking its eight seconds: the halt is among the
        # modules, and the UDP shell still answers
        refused = ""
        try:
            refused = sh.run("insmod /hello.ko")
        except Exception as e:
            refused = "no answer: %s" % e
        pt.check("an insmod while the halt unloads the modules is refused: the machine is going down",
                 "going down" in refused, refused)

        for _ in range(OFF_TIMEOUT * 2):
            if p.poll() is not None:
                break
            time.sleep(0.5)
        pt.check("and the machine switches itself off", p.poll() is not None)
    finally:
        if p.poll() is None:
            pt.kill(p)
        out.close()

    said = text()
    pt.check("what the modules' exits say is on the console, which the shell had kept the log off",
             "slowexit: exit takes" in said and "slowexit: exit done" in said and "modtest: unloaded" in said,
             said[-1500:])
    pt.check("hello was not loaded behind the halt", "module: hello loaded" not in said)
    pt.check("the shells were stopped after the modules, and the root unmounted after them",
             0 <= said.find("slowexit: exit done") < said.find("unmounting /"), said[-1500:])
    pt.check("nothing panicked, the way down included", "PANIC" not in said,
             "\n".join(l for l in said.splitlines() if "PANIC" in l))

    if args.keep:
        print("left at " + tmp)
    else:
        shutil.rmtree(tmp, ignore_errors=True)

    print()
    if pt.failures:
        print("FAILED: " + ", ".join(pt.failures))
        return 1
    print("halt-test: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
