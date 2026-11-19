#!/usr/bin/env python3
"""A host's PCI function given to a Linux guest of nos's, with no GPU to
hand: QEMU's NVMe behind a PCIe root port, QEMU's VT-d in front of it
(`-device intel-iommu,intremap=on`), nos taking the unit over (`iommu=on`),
keeping the NVMe from its own driver (`guestpci=01:00.0`) and giving it to its
guest (`hv boot ... pci=01:00.0`) -- MSI-X's table trapped, its vectors the
host's, its DMA through a domain of the guest's RAM (plans/gpu-passthrough.md).

- nos boots under TCG, keeps the function from its drivers, takes the unit
  over with interrupts remapped, and gives the function to its guest by its
  MSI-X;
- the guest's own driver finds the NVMe and reads what the runner put on it
  (the device's DMA into the guest's RAM, through the domain), writes a
  pattern of its own and reads it back past the page cache (the guest's RAM
  to the device and back), by MSI-X interrupts -- the vectors nos sends on,
  more than one -- and the pattern is on the disk's image once QEMU is gone;
- the hostile phase: the guest lets its driver go and drives the controller
  by hand (tests/hv-passthrough/nvmepoke.c), its DMA pointed at a page of the
  guest's (answered), at an address where the guest has no RAM (the write
  blocked, said by nos with the NVMe's requester id -- QEMU's NVMe completes
  the command as if it went) -- and at a page of the guest's again: the
  controller still answers, and nos goes on; and at the interrupt range, in
  the remappable format, naming an entry nos never filled and one past its
  table: each refused by the table, the fault said (QEMU's VT-d delivers a
  compatibility-format message whatever CFI says, and gives a DMA's message
  no requester id: those two only the metal shows);
- hv sent the device's MSIs on, every one taken, and the guest stops.

The guest's kernel is a 64-bit bzImage with PCI, MSI, NVMe (BLK_DEV_NVME),
devtmpfs, /proc/self/pagemap (PROC_PAGE_MONITOR) and an 8250 console --
docs/testing.md has the recipe; its root is built here, from a static
busybox for x86-64 (`--busybox`: the binary, or an initramfs that holds it as
bin/busybox) and nvmepoke, built static (`--nvmepoke`, or by the host's `cc
-static` on x86-64 Linux):

    make nocheck && make nos-test.iso TEST_ISO_ARGS="iommu=on guestpci=01:00.0"
    tests/hv-passthrough-test.py --bzimage <bzImage> --busybox <busybox> [--nvmepoke <nvmepoke>]

x86-64, TCG: the guest twice emulated, some minutes. Manual: not in CI.
Exit code 0 = every check passed.
"""

import argparse
import gzip
import hashlib
import importlib.util
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

spec = importlib.util.spec_from_file_location("pt", os.path.join(HERE, "parttest.py"))
pt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pt)

FILES = os.path.join(HERE, "hv-passthrough")
# Where the NVMe is in nos: behind the root port, alone on its bus
NVME_IN_NOS = "01:00.0"
# The guest: its RAM, its CPUs -- two, for the NVMe's queues a vector each --
# and how long it may run
GUEST_MIB = 256
GUEST_CPUS = 2
GUEST_SECS = 600
DEADLINE_SECS = 1500
# The NVMe's image: a pattern of the runner's in its first 64 KiB, the
# guest's written at block 256 (of 4 KiB)
IMAGE_BYTES = 8 << 20
PATTERN_BYTES = 64 << 10
WRITE_AT = 256 * 4096
GUEST_WROTE = b"nos gave this guest the NVMe and it wrote this\n"
# Where the guest has no RAM: its 256 MiB are below it
NO_RAM = 0x40000000
# A message the device writes: the Identify's first 16 bytes at the end of
# the interrupt range's second page, four dwords each in the remappable
# format (bit 4) naming entry 255 -- the last of nos's 256, which it hands out
# from the first on -- or past the table; the rest to a page of the guest's.
# QEMU's VT-d delivers a compatibility-format message whatever CFI says, and
# takes a DMA's messages as no requester's (SID 0xffff): what it shows of nos
# is the table in force and its faults said, not CFI's block nor the
# requester's check -- those are the metal's
TO_MESSAGES = "0xfee01ff0+buf"
# QEMU's NVMe's vendor, as Identify Controller says it
QEMU_NVME_VENDOR = "0x1b36"
# The last line of /etc/rc, and how the console shows it ran (hv-linux-test.py)
RC_DONE = re.compile(r"(?m)^> version\n.+\n")


def pattern():
    out = bytearray()
    n = 0
    while len(out) < PATTERN_BYTES:
        out += hashlib.sha256(b"hv-passthrough %d" % n).digest()
        n += 1
    return bytes(out[:PATTERN_BYTES])


def guest_wrote():
    want = (GUEST_WROTE * (65536 // len(GUEST_WROTE) + 1))[:65536]
    return want


def newc(entries):
    """A cpio archive of newc entries -- (name, mode, bytes, (major,
    minor)) -- and its trailer: an initramfs Linux unpacks."""
    out = bytearray()
    for ino, (name, mode, data, (major, minor)) in enumerate(entries + [("TRAILER!!!", 0, b"", (0, 0))], 1):
        nm = name.encode() + b"\0"
        fields = (ino, mode, 0, 0, 1, 0, len(data), 0, 0, major, minor, len(nm), 0)
        out += b"070701" + "".join("%08x" % v for v in fields).encode() + nm
        out += b"\0" * (-len(out) % 4)
        out += data
        out += b"\0" * (-len(out) % 4)
    return bytes(out)


def unnewc(blob, wanted):
    """The bytes of `wanted` in a newc archive, None where it is not one."""
    at = 0
    while at + 110 <= len(blob) and blob[at:at + 6] == b"070701":
        size = int(blob[at + 54:at + 62], 16)
        namesize = int(blob[at + 94:at + 102], 16)
        name = blob[at + 110:at + 110 + namesize - 1].decode(errors="replace")
        data = at + 110 + namesize
        data += -data % 4
        if name == "TRAILER!!!":
            return None
        if name.lstrip("./") == wanted:
            return blob[data:data + size]
        at = data + size
        at += -at % 4
    return None


def busybox(path):
    """A static busybox for x86-64: the file, or bin/busybox of an initramfs."""
    blob = open(path, "rb").read()
    if blob[:2] == b"\x1f\x8b":
        blob = gzip.decompress(blob)
    if blob[:6] == b"070701":
        blob = unnewc(blob, "bin/busybox")
    if blob is None or blob[:4] != b"\x7fELF" or blob[18:20] != b"\x3e\x00":
        return None
    return blob


def nvmepoke(args, tmp):
    """nvmepoke built static for x86-64: --nvmepoke's, or the host's cc's."""
    if args.nvmepoke:
        return open(args.nvmepoke, "rb").read()
    if platform.system() != "Linux" or platform.machine() not in ("x86_64", "AMD64"):
        return None
    out = os.path.join(tmp, "nvmepoke")
    p = subprocess.run(["cc", "-static", "-O2", "-Wall", "-Wextra", "-o", out, os.path.join(FILES, "nvmepoke.c")],
                       capture_output=True, text=True)
    if p.returncode != 0:
        print(p.stdout + p.stderr)
        return None
    return open(out, "rb").read()


def initrd(bb, poke, hostile):
    """The guest's initramfs: busybox, nvmepoke, the targets of the hostile
    phase, and /init (guest-init)."""
    d, f, c = 0o040755, 0o100755, 0o020600
    entries = [("bin", d, b"", (0, 0)), ("dev", d, b"", (0, 0)), ("proc", d, b"", (0, 0)),
               ("sys", d, b"", (0, 0)), ("tmp", d, b"", (0, 0)),
               ("dev/console", c, b"", (5, 1)),
               ("bin/busybox", f, bb, (0, 0)),
               ("bin/nvmepoke", f, poke, (0, 0)),
               ("hostile", 0o100644, (" ".join(hostile) + "\n").encode(), (0, 0)),
               ("init", f, open(os.path.join(FILES, "guest-init"), "rb").read(), (0, 0))]
    return gzip.compress(newc(entries))


def lines(text, word):
    return "\n".join(l for l in text.splitlines() if word in l)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bzimage", required=True, help="the guest's kernel")
    ap.add_argument("--busybox", required=True, help="a static busybox for x86-64, or an initramfs holding one")
    ap.add_argument("--nvmepoke", help="tests/hv-passthrough/nvmepoke.c built static for x86-64")
    ap.add_argument("--iso", default=os.path.join(ROOT, "nos-test.iso"))
    ap.add_argument("--deadline", type=int, default=DEADLINE_SECS, help="seconds the whole run may take")
    ap.add_argument("--keep", action="store_true", help="keep the serial log")
    args = ap.parse_args()

    hv = os.path.join(ROOT, "out", "x86_64", "modules", "hv.ko")
    for f in (args.iso, hv, args.bzimage, args.busybox):
        if not os.path.exists(f):
            print("hv-passthrough-test: no %s -- see the usage" % f)
            return 2
    tmp = tempfile.mkdtemp(prefix="hv-passthrough-")
    bb = busybox(args.busybox)
    poke = nvmepoke(args, tmp)
    if bb is None or poke is None:
        print("hv-passthrough-test: %s -- see the usage" % ("no static busybox for x86-64 in " + args.busybox
                                                            if bb is None else "no nvmepoke: --nvmepoke"))
        return 2

    hostile = ["buf", "%#x" % NO_RAM, TO_MESSAGES, "buf"]
    rootdir = os.path.join(tmp, "rootdir")
    os.makedirs(os.path.join(rootdir, "etc"))
    shutil.copy(hv, rootdir)
    shutil.copy(args.bzimage, os.path.join(rootdir, "bzImage"))
    with open(os.path.join(rootdir, "initrd"), "wb") as f:
        f.write(initrd(bb, poke, hostile))
    rc = ["insmod /hv.ko",
          "hv on",
          "hv boot /bzImage initrd=/initrd mem=%d cpus=%d secs=%d pci=%s cmdline=console=ttyS0 rdinit=/init"
          % (GUEST_MIB, GUEST_CPUS, GUEST_SECS, NVME_IN_NOS),
          "dmesg vtd:",
          "dmesg hv:",
          "iommu",
          "version"]
    with open(os.path.join(rootdir, "etc", "rc"), "w") as f:
        f.write("# tests/hv-passthrough-test.py\n" + "\n".join(rc) + "\n")
    root = os.path.join(tmp, "root.img")
    subprocess.run([os.path.join(ROOT, "scripts", "mkrootfs.sh"), root, "64", rootdir], cwd=ROOT, check=True,
                   stdout=subprocess.DEVNULL)
    image = os.path.join(tmp, "nvme.img")
    with open(image, "wb") as f:
        f.write(pattern())
        f.truncate(IMAGE_BYTES)

    log = os.path.join(tmp, "nos.log")
    argv = ["qemu-system-x86_64", "-machine", "q35", "-device", "intel-iommu,intremap=on",
            "-cpu", "max", "-smp", "2", "-m", "2G", "-display", "none", "-serial", "file:" + log,
            "-cdrom", args.iso,
            "-drive", "file=%s,format=raw,if=none,id=root" % root,
            "-device", "virtio-blk-pci,drive=root,disable-legacy=on,disable-modern=off",
            "-device", "pcie-root-port,id=rp1,chassis=1",
            "-drive", "file=%s,format=raw,if=none,id=pass" % image,
            "-device", "nvme,drive=pass,serial=passthrough,bus=rp1"]
    p = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)
    t0 = time.time()
    text = ""
    try:
        while time.time() - t0 < args.deadline and p.poll() is None:
            time.sleep(5)
            text = open(log, errors="replace").read() if os.path.exists(log) else ""
            if "PANIC" in text or RC_DONE.search(text):
                break
    finally:
        p.kill()
        p.wait()
    text = open(log, errors="replace").read() if os.path.exists(log) else ""
    print("  %d s" % (time.time() - t0))

    nos_checks(text)
    guest_checks(text)
    hostile_checks(text)
    disk = open(image, "rb").read()
    pt.check("the guest's pattern is on the disk's image", disk[WRITE_AT:WRITE_AT + 65536] == guest_wrote(),
             repr(disk[WRITE_AT:WRITE_AT + 64]))
    return finish(args, tmp, text)


def nos_checks(text):
    pt.check("nos boots", "boot: complete" in text, text[-2000:])
    pt.check("nos did not panic", "PANIC" not in text, text[text.find("PANIC"):][:2000])
    pt.check("the NVMe is kept from nos's drivers", "pci: %s kept for guests" % NVME_IN_NOS in text,
             lines(text, "pci:"))
    pt.check("QEMU's unit is taken over, every device passed through", "passed through, queued invalidation" in text,
             lines(text, "vtd:"))
    pt.check("interrupts are remapped", "interrupts remapped, the IO-APIC's from" in text, lines(text, "vtd:"))
    given = re.search(r"hv: %s given to a guest .*interrupts \(MSI-X, (\d+) of its vectors\) remapped" % NVME_IN_NOS,
                      text)
    pt.check("hv gives the NVMe by its MSI-X, its interrupts remapped", given is not None, lines(text, "hv: "))
    if given:
        print("  MSI-X: %s vectors given" % given.group(1))


def guest_checks(text):
    want = hashlib.md5(pattern()).hexdigest()
    pt.check("the guest boots", "pass-guest: up:" in text, text[-3000:])
    pt.check("the guest's driver finds the NVMe", re.search(r"pass-guest: namespace of \d+ sectors", text) is not None,
             lines(text, "pass-guest:"))
    pt.check("it reads what the runner put there", "pass-guest: read %s" % want in text, lines(text, "pass-guest: read"))
    wrote = hashlib.md5(guest_wrote()).hexdigest()
    pt.check("it writes, and reads back past the page cache", "pass-guest: wrote %s" % wrote in text
             and "pass-guest: read back %s" % wrote in text, lines(text, "pass-guest: "))
    irqs = re.findall(r"pass-guest: irq\s+\d+:\s+((?:\d+\s+)+)", text)
    counts = [sum(int(n) for n in row.split()) for row in irqs]
    pt.check("by MSI-X interrupts, more than one vector taking them", len([c for c in counts if c > 0]) >= 2,
             lines(text, "pass-guest: irq"))
    msis = re.search(r"host fn\s+(\d+) MSIs of its host's functions sent on to it.*?, (\d+) of them taken by no CPU",
                     text)
    pt.check("hv sent the NVMe's MSIs on, every one taken", msis is not None and int(msis.group(1)) > 0
             and msis.group(2) == "0", msis.group(0) if msis else lines(text, "host fn"))
    pt.check("the guest stops", re.search(r"(?m)^  stopped", text) is not None, lines(text, "stopped"))


def hostile_checks(text):
    ran = re.findall(r"pass-guest: nvmepoke: (\S+) (status (0x[0-9a-f]+|0)|timeout|vendor (0x[0-9a-f]+))", text)
    statuses = [(where, int(s, 16)) for where, _, s, _ in ran if s]
    vendors = [v for _, _, _, v in ran if v]
    pt.check("the guest drives the controller by hand", "pass-guest: nvmepoke: up" in text,
             lines(text, "nvmepoke"))
    pt.check("a page of the guest's is written by the device", len(statuses) >= 1 and statuses[0] == ("buf", 0)
             and vendors[:1] == [QEMU_NVME_VENDOR], lines(text, "nvmepoke"))
    no_ram = [s for where, s in statuses if where == "%#x" % NO_RAM]
    pt.check("the device is told to write where the guest has no RAM", no_ram[:1] != [], lines(text, "nvmepoke"))
    said = re.search(r"vtd: dmar\d+: write of 0x%X by %s blocked: (0x[0-9a-f]+), (.*)" % (NO_RAM, NVME_IN_NOS), text)
    pt.check("the write is blocked, said by nos with the NVMe's id", said is not None, lines(text, "vtd: dmar"))
    if said and no_ram:
        # QEMU's NVMe completes the command as if its data went: its DMA's
        # failure is the IOMMU's to say, not the device's
        print("  blocked: %s, %s -- the device's status %#x" % (said.group(1), said.group(2), no_ram[0]))
    pt.check("the device is told to write messages", any(where == TO_MESSAGES for where, _ in statuses),
             lines(text, "nvmepoke"))
    refused = re.search(r"vtd: dmar\d+: interrupt 0x([0-9A-F]+) from (\S+) blocked: (0x2[0-6]), (.*)", text)
    pt.check("each is refused by nos's table, said", refused is not None and int(refused.group(1), 16) >= 255,
             lines(text, "vtd: dmar"))
    if refused:
        print("  message %s: %s, %s (from %s: QEMU gives a DMA's message no requester)" % (
            refused.group(1), refused.group(3), refused.group(4), refused.group(2)))
    pt.check("the controller answers after it", len(statuses) >= 4 and statuses[3] == ("buf", 0)
             and len(vendors) >= 2 and vendors[1] == QEMU_NVME_VENDOR, lines(text, "nvmepoke"))
    pt.check("the guest goes on to its end", "pass-guest: done" in text, lines(text, "pass-guest:")[-2000:])


def finish(args, tmp, text):
    if args.keep or pt.failures:
        path = os.path.join(tmp, "nos.log")
        print("the serial log in " + path)
    else:
        shutil.rmtree(tmp, ignore_errors=True)
    print()
    if pt.failures:
        print("FAILED: " + ", ".join(pt.failures))
        return 1
    print("hv-passthrough-test: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
