#!/usr/bin/env python3
"""A host's GPU given to a Linux guest of nos's (plans/gpu-passthrough.md),
on a machine that has one: nos under QEMU/KVM there, QEMU's VT-d in front of
the real GPU (vfio-pci), nos taking the unit over with iommu=on and giving
the GPU to its own guest (`hv boot ... pci=01:00.0 unsafe_interrupts`) --
the machine's own kernel, booted from an initramfs of busybox, nouveau and
the GPU's firmware (tests/gpu-passthrough/). One run of the guest:

- nos boots, takes QEMU's unit over, and does not panic;
- the guest sees the GPU on its bus, BAR0 and BAR1 where nos put them --
  BAR1 an A2000's 8 GiB, whose low register has no address bit -- and reads
  BOOT0 from BAR0: the GPU's own register, through the nested table;
- nouveau comes up: the GSP's firmware booted by DMA through nos's IOMMU
  domain of the guest's RAM, VRAM sized, "[drm] Initialized";
- a thousand fences, each an EXEC of nothing waited for on its syncobj
  (fence.c): a wait only the GPU's interrupt ends -- the host's MSI sent on
  by hv: every one signalled, and hv says it sent the MSIs on, none of them
  taken by no CPU. A fence at a time does not show the MSI re-arm the
  guest writes through the mirror (a dword at 0x704, Pascal on) to be
  needed: with it dropped, an A2000's thousand fences still signalled --
  hv-fuzz's hostfn target holds that write to reaching the real mirror;
- the guest powers itself off.

The machine (`--host`, as ssh reaches it, root) is the plan's Hertz or one
like it: KVM with nested=Y, the GPU bound to vfio-pci (its other functions
too, unused), its kernel's nouveau and linux-firmware's for the GPU,
busybox, gcc, zstd, cpio and e2fsprogs. What runs there runs in a directory
of its own (`--dir`), QEMU as a transient systemd unit, stopped at the end.

    make nocheck && make nos-test.iso TEST_ISO_ARGS="iommu=on udpshell=9100 hugepages=5G"
    tests/gpu-passthrough-test.py --host root@<machine> [--gpu 0000:01:00.0] [--chip ga106] [--keep]

x86-64, Intel VT-d. Manual: not in CI. Exit code 0 = every check passed.
"""

import argparse
import importlib.util
import os
import re
import shlex
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

spec = importlib.util.spec_from_file_location("pt", os.path.join(HERE, "parttest.py"))
pt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pt)

FILES = os.path.join(HERE, "gpu-passthrough")
# Where the GPU is in nos: behind the root port run-qemu.sh gives it
GPU_IN_NOS = "01:00.0"
UNIT = "nos-gpu-passthrough-test"
# The guest's RAM, out of nos's huge pool (hugepages=5G): 2 MiB leaves in
# the domain, so few mappings for QEMU's VFIO to follow; nos's own memory
NOS_MEM = "10G"
GUEST_MIB = 4096
GUEST_SECS = 300
# How long the whole run may take: nos's boot, the initramfs streamed in,
# nouveau's GSP, the fences
DEADLINE_SECS = 420
FENCES = 1000


def ssh(args, command, timeout=120):
    """`command` on the machine; its output, and whether it succeeded."""
    p = subprocess.run(["ssh", "-n", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15", args.host, command],
                       capture_output=True, text=True, timeout=timeout)
    return p.stdout + p.stderr, p.returncode == 0


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--host", required=True, help="the machine with the GPU, as ssh reaches it (root)")
    ap.add_argument("--gpu", default="0000:01:00.0", help="the GPU on the machine, bound to vfio-pci")
    ap.add_argument("--chip", default="ga106", help="its firmware's directory under /lib/firmware/nvidia")
    ap.add_argument("--dir", default="/root/nos-gpu-passthrough-test", help="where the run lives on the machine")
    ap.add_argument("--iso", default=os.path.join(ROOT, "nos-test.iso"))
    ap.add_argument("--keep", action="store_true", help="keep the serial log here")
    args = ap.parse_args()

    hv = os.path.join(ROOT, "out", "x86_64", "modules", "hv.ko")
    for f in (args.iso, hv):
        if not os.path.exists(f):
            print("gpu-passthrough-test: no %s -- see the usage" % f)
            return 2
    d = shlex.quote(args.dir)

    _, ok = ssh(args, "systemctl stop %s 2>/dev/null; mkdir -p %s" % (UNIT, d))
    if not pt.check("the machine answers", ok):
        return finish(args, None)
    sent = subprocess.run(["scp", "-q", "-o", "BatchMode=yes", args.iso, hv,
                           os.path.join(ROOT, "scripts", "mkrootfs.sh"), os.path.join(FILES, "guest-init"),
                           os.path.join(FILES, "fence.c"), os.path.join(FILES, "make-root.sh"),
                           os.path.join(FILES, "run-qemu.sh"), "%s:%s/" % (args.host, args.dir)])
    if not pt.check("the files reach it", sent.returncode == 0):
        return finish(args, None)
    out, ok = ssh(args, "cd %s && bash ./make-root.sh %s %s %s %d %d" % (
        d, d, GPU_IN_NOS, shlex.quote(args.chip), GUEST_MIB, GUEST_SECS), timeout=600)
    if not pt.check("the guest and nos's root are made there", ok, out[-2000:]):
        return finish(args, None)
    print("  " + out.strip().splitlines()[-1])

    start = ("cd %s && rm -f nos.log && systemd-run --unit=%s --collect --quiet --working-directory=%s "
             "bash ./run-qemu.sh nos-test.iso nos.log %s %s "
             "-drive file=guest/root.img,format=raw,if=none,id=d0 -device virtio-blk-pci,drive=d0") % (
        d, UNIT, d, shlex.quote(args.gpu), NOS_MEM)
    out, ok = ssh(args, start)
    if not pt.check("nos starts under QEMU there", ok, out):
        return finish(args, None)
    wait = ("for i in $(seq 1 %d); do grep -aqE 'gpu-guest: done|PANIC|boot failed|end ttyS0' %s/nos.log 2>/dev/null "
            "&& break; systemctl is-active -q %s || break; sleep 5; done; cat %s/nos.log") % (
        DEADLINE_SECS // 5, d, UNIT, d)
    text, _ = ssh(args, wait, timeout=DEADLINE_SECS + 60)
    ssh(args, "systemctl stop %s" % UNIT)
    checks(text)
    return finish(args, text)


def checks(text):
    pt.check("nos boots", "boot: complete" in text, text[-2000:])
    pt.check("nos did not panic", "PANIC" not in text, text[text.find("PANIC"):][:2000])
    pt.check("QEMU's unit is taken over, every device passed through", "passed through, queued invalidation" in text)
    pt.check("the guest sees the GPU on its bus", re.search(r"gpu-guest: pci \S+ 0x10de:", text) is not None,
             "\n".join(l for l in text.splitlines() if l.startswith("gpu-guest: pci")))
    resources = re.findall(r"gpu-guest: resource (0x[0-9a-f]+) (0x[0-9a-f]+)", text)
    sized = [int(e, 16) - int(s, 16) + 1 if int(s, 16) else 0 for s, e in resources]
    pt.check("BAR0 and BAR1 where nos put them", len(sized) >= 2 and sized[0] > 0 and sized[1] > 0,
             "\n".join("BAR%d %#x bytes" % (i, n) for i, n in enumerate(sized)))
    boot0 = re.search(r"gpu-guest: BOOT0 0x([0-9A-Fa-f]+)", text)
    value = int(boot0.group(1), 16) if boot0 else 0
    pt.check("BOOT0 read from BAR0, the GPU's own register", value not in (0, 0xFFFFFFFF), hex(value))
    pt.check("nouveau comes up: the GSP booted through the domain", "[drm] Initialized nouveau" in text,
             "\n".join(l for l in text.splitlines() if "nouveau" in l)[-3000:])
    fences = re.search(r"gpu-fence: (\d+) of (\d+) fences signalled, (\d+) us each on average", text)
    pt.check("every fence signalled on the GPU's interrupt",
             fences is not None and fences.group(1) == fences.group(2) == str(FENCES),
             fences.group(0) if fences else "\n".join(l for l in text.splitlines() if "fence" in l))
    if fences:
        print("  %s fences, %s us each" % (fences.group(1), fences.group(3)))
    msis = re.search(r"host fn\s+(\d+) MSIs of its host's functions sent on to it.*?, (\d+) of them taken by no CPU",
                     text)
    pt.check("hv sent the GPU's MSIs on, every one taken", msis is not None and int(msis.group(1)) > 0
             and msis.group(2) == "0", msis.group(0) if msis else "no line of hv's for them")
    pt.check("the guest powers itself off", "the guest powered itself off" in text,
             "\n".join(l for l in text.splitlines() if l.startswith("  stopped")))


def finish(args, text):
    if text is not None and (args.keep or pt.failures):
        fd, path = tempfile.mkstemp(prefix="gpu-passthrough-", suffix=".log")
        with os.fdopen(fd, "w") as f:
            f.write(text)
        print("the serial log in " + path)
    print()
    if pt.failures:
        print("FAILED: " + ", ".join(pt.failures))
        return 1
    print("gpu-passthrough-test: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
