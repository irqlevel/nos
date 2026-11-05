#!/usr/bin/env python3
"""What TLS 1.3 costs this machine's CPUs inside nos (plans/ipc.md, I4).

Boots nos with tlsbench.ko on its root and an /etc/rc that loads it, runs
`tlsbench` and powers off; prints what tlsbench said. tlsbench runs both
pinned ends of `tls::pinned` in one kernel over a pipe in memory, so the
numbers are the CPUs' -- the handshake, and MB/s each way per cipher suite
-- and nothing of a network's.

    scripts/tlsbench.py                     x86-64: KVM where the host has it, TCG otherwise
    scripts/tlsbench.py --arch aarch64      arm64: HVF on an Apple Silicon Mac, TCG otherwise
    scripts/tlsbench.py --mb 64 --suite chacha --cpus 2

Run on the servers under KVM -- nos as a guest, nothing rebooted -- it says
what their cores give a TLS session with no SSE; under TCG it says nothing
worth quoting. Needs a built tree: nos.iso (x86-64) or nos-arm64.img, and
out/<arch>/modules/tlsbench.ko.
"""

import argparse
import os
import platform
import re
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)


def kvm_usable():
    """Whether QEMU can use KVM here: the device, and a CPU with the extension."""
    if not os.path.exists("/dev/kvm"):
        return False
    try:
        with open("/proc/cpuinfo") as f:
            flags = f.read()
    except OSError:
        return False
    return re.search(r"^flags\s*:.*\b(svm|vmx)\b", flags, re.M) is not None


def rootfs(tmp, arch, rc):
    rootdir = os.path.join(tmp, "rootdir")
    os.makedirs(os.path.join(rootdir, "etc"))
    ko = os.path.join(ROOT, "out", arch, "modules", "tlsbench.ko")
    if not os.path.exists(ko):
        sys.exit("no %s -- build the tree first (make ARCH=%s)" % (ko, arch))
    shutil.copy(ko, rootdir)
    with open(os.path.join(rootdir, "etc", "rc"), "w") as f:
        f.write("# scripts/tlsbench.py\n" + "\n".join(rc) + "\n")
    image = os.path.join(tmp, "root.img")
    subprocess.run([os.path.join(HERE, "mkrootfs.sh"), image, "64", rootdir, "1024"],
                   cwd=ROOT, check=True, stdout=subprocess.DEVNULL)
    return image


def argv_x86(image, log, cpus, accel):
    iso = os.path.join(ROOT, "nos.iso")
    if not os.path.exists(iso):
        sys.exit("no nos.iso -- make first")
    argv = ["qemu-system-x86_64", "-display", "none", "-m", "1G", "-smp", str(cpus),
            "-cdrom", iso, "-serial", "file:" + log,
            "-drive", "file=%s,format=raw,id=drive0,if=none" % image,
            "-device", "virtio-blk-pci,drive=drive0,disable-legacy=on,disable-modern=off",
            "-device", "virtio-rng-pci"]
    if accel == "kvm":
        argv += ["-enable-kvm", "-cpu", "host"]
    else:
        argv += ["-cpu", "max"]
    return argv


def argv_arm64(image, log, cpus, accel):
    img = os.path.join(ROOT, "nos-arm64.img")
    if not os.path.exists(img):
        sys.exit("no nos-arm64.img -- make ARCH=aarch64 first")
    cpu = {"hvf": ["-accel", "hvf", "-cpu", "host"], "kvm": ["-accel", "kvm", "-cpu", "host"]}.get(
        accel, ["-accel", "tcg", "-cpu", "cortex-a72"])
    return ["qemu-system-aarch64", "-M", "virt,gic-version=3", "-smp", str(cpus), "-m", "1024"] + cpu + [
        "-kernel", img, "-append", "root=auto",
        "-global", "virtio-mmio.force-legacy=false",
        "-drive", "file=%s,format=raw,id=hd0,if=none" % image,
        "-device", "virtio-blk-device,drive=hd0",
        "-device", "virtio-rng-device",
        "-serial", "file:" + log, "-display", "none"]


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--arch", choices=["x86_64", "aarch64"], default="x86_64")
    ap.add_argument("--mb", type=int, default=16)
    ap.add_argument("--suite", choices=["all", "aes128", "aes256", "chacha"], default="all")
    ap.add_argument("--cpus", type=int, default=2)
    ap.add_argument("--timeout", type=int, default=1800, help="seconds the whole run may take")
    ap.add_argument("--keep", action="store_true", help="keep the serial log")
    args = ap.parse_args()

    if args.arch == "x86_64":
        accel = "kvm" if kvm_usable() and platform.machine() in ("x86_64", "AMD64") else "tcg"
    elif platform.system() == "Darwin" and platform.machine() == "arm64":
        accel = "hvf"
    elif kvm_usable() and platform.machine() == "aarch64":
        accel = "kvm"
    else:
        accel = "tcg"

    cmd = "tlsbench mb=%d suite=%s" % (args.mb, args.suite)
    rc = ["insmod /tlsbench.ko", cmd, "poweroff"]
    tmp = tempfile.mkdtemp(prefix="nos-tlsbench-")
    log = os.path.join(tmp, "serial.log")
    image = rootfs(tmp, args.arch, rc)
    argv = (argv_x86 if args.arch == "x86_64" else argv_arm64)(image, log, args.cpus, accel)
    print("tlsbench.py: %s, %s, %d CPUs: %s" % (args.arch, accel, args.cpus, cmd), flush=True)
    p = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    ok = False
    try:
        end = time.time() + args.timeout
        while time.time() < end and p.poll() is None:
            time.sleep(1)
        if p.poll() is None:
            print("tlsbench.py: no power-off within %d s" % args.timeout)
        text = open(log, errors="replace").read() if os.path.exists(log) else ""
        # What the command printed: from its echo to the next command's
        m = re.search(r"(?ms)^> %s\n(.*?)(?=^> |\Z)" % re.escape(cmd), text)
        if m:
            body = m.group(1).rstrip()
            print(body)
            ok = "MB/s" in body and "tlsbench:" in body and "refused" not in body
        else:
            print("tlsbench.py: the command's output is not in the log")
        if "PANIC" in text:
            print("tlsbench.py: the kernel panicked")
            ok = False
    finally:
        if p.poll() is None:
            p.kill()
            p.wait()
        if args.keep or not ok:
            print("log at " + log)
        else:
            shutil.rmtree(tmp, ignore_errors=True)
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
