#!/usr/bin/env python3
"""A host's PCI function given to a Linux guest of nos's, with no GPU to
hand: QEMU's NVMe and a virtio disk, each behind a PCIe root port, QEMU's
VT-d in front of them (`-device intel-iommu,intremap=on`), nos taking the unit
over (`iommu=on`), keeping both from its own drivers (`guestpci=01:00.0,02:00.0`)
and giving them to its guest (`hv boot ... pci=01:00.0 pci=02:00.0`) -- MSI-X's
tables trapped, their vectors the host's, their DMA through a domain of the
guest's RAM (plans/gpu-passthrough.md). The virtio disk has no FLR: the host
resets it by its bridge's secondary bus reset.

- nos boots under TCG, keeps the function from its drivers, takes the unit
  over with interrupts remapped, and gives the function to its guest by its
  MSI-X;
- the guest's own drivers find both and read what the runner put on each
  (the devices' DMA into the guest's RAM, through the domain), write a
  pattern of their own and read it back past the page cache (the guest's RAM
  to the device and back), by MSI-X interrupts -- the vectors nos sends on,
  the NVMe's more than one -- and the patterns are on the disks' images once
  QEMU is gone; the virtio disk reset by its bridge as it is given and as
  the guest lets go of it;
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
- the NVMe's expansion ROM (a 64 KiB image of the runner's, `romfile=`) is
  read by the host and given a copy: the guest reads it through sysfs, the
  image byte for byte;
- hv sent the device's MSIs on, every one taken, and the guest stops.

The guest's kernel is a 64-bit bzImage with PCI, MSI, NVMe (BLK_DEV_NVME),
devtmpfs, /proc/self/pagemap (PROC_PAGE_MONITOR) and an 8250 console --
docs/testing.md has the recipe; its root is built here, from a static
busybox for x86-64 (`--busybox`: the binary, or an initramfs that holds it as
bin/busybox) and nvmepoke, built static (`--nvmepoke`, or by the host's `cc
-static` on x86-64 Linux):

    make nocheck && make nos-test.iso TEST_ISO_ARGS="iommu=on guestpci=01:00.0,02:00.0"
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
# Where the two functions are in nos: each behind a root port, alone on its
# bus -- the NVMe, and a virtio disk with no FLR (`x-pcie-flr-init=off`),
# which the host resets by its bridge's secondary bus reset
NVME_IN_NOS = "01:00.0"
VIRTIO_IN_NOS = "02:00.0"
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
# The NVMe's expansion ROM (`romfile=`): an image of the PCI Firmware spec's
# (5.1) the runner makes -- its 0x55AA, its PCI data structure, code type
# EFI so that SeaBIOS runs none of it -- of 64 KiB, the rest a pattern
ROM_BYTES = 64 << 10
ROM_PCIR = 0x1C
# The last line of /etc/rc, and how the console shows it ran (hv-linux-test.py)
RC_DONE = re.compile(r"(?m)^> version\n.+\n")


def pattern(name):
    out = bytearray()
    n = 0
    while len(out) < PATTERN_BYTES:
        out += hashlib.sha256(b"hv-passthrough %s %d" % (name.encode(), n)).digest()
        n += 1
    return bytes(out[:PATTERN_BYTES])


def guest_wrote(name):
    """What guest-init writes at block 256 of the disk it calls `name`."""
    line = b"nos gave this guest the %s and it wrote this\n" % name.encode()
    return (line * (65536 // len(line) + 1))[:65536]


def rom():
    """The NVMe's ROM image: a header, a PCI data structure naming QEMU's
    NVMe, the last image, of EFI code; a pattern after."""
    out = bytearray(pattern("rom")[:ROM_BYTES] * (ROM_BYTES // PATTERN_BYTES + 1))[:ROM_BYTES]
    units = ROM_BYTES // 512
    out[0:2] = b"\x55\xAA"
    out[2] = min(units, 0xFF)
    out[0x18:0x1A] = ROM_PCIR.to_bytes(2, "little")
    pcir = b"PCIR" + (0x1B36).to_bytes(2, "little") + (0x0010).to_bytes(2, "little") + b"\0\0" \
        + (0x18).to_bytes(2, "little") + b"\0" + bytes([0x02, 0x08, 0x01]) + units.to_bytes(2, "little") \
        + b"\0\0" + bytes([0x03, 0x80]) + b"\0\0"
    out[ROM_PCIR:ROM_PCIR + len(pcir)] = pcir
    return bytes(out)


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
          "hv boot /bzImage initrd=/initrd mem=%d cpus=%d secs=%d pci=%s pci=%s cmdline=console=ttyS0 rdinit=/init"
          % (GUEST_MIB, GUEST_CPUS, GUEST_SECS, NVME_IN_NOS, VIRTIO_IN_NOS),
          "dmesg vtd:",
          "dmesg hv:",
          "iommu",
          "version"]
    with open(os.path.join(rootdir, "etc", "rc"), "w") as f:
        f.write("# tests/hv-passthrough-test.py\n" + "\n".join(rc) + "\n")
    root = os.path.join(tmp, "root.img")
    subprocess.run([os.path.join(ROOT, "scripts", "mkrootfs.sh"), root, "64", rootdir], cwd=ROOT, check=True,
                   stdout=subprocess.DEVNULL)
    images = {}
    for name in ("nvme", "virtio"):
        images[name] = os.path.join(tmp, name + ".img")
        with open(images[name], "wb") as f:
            f.write(pattern(name))
            f.truncate(IMAGE_BYTES)

    rom_path = os.path.join(tmp, "nvme.rom")
    with open(rom_path, "wb") as f:
        f.write(rom())
    log = os.path.join(tmp, "nos.log")
    argv = ["qemu-system-x86_64", "-machine", "q35", "-device", "intel-iommu,intremap=on",
            "-cpu", "max", "-smp", "2", "-m", "2G", "-display", "none", "-serial", "file:" + log,
            "-cdrom", args.iso,
            "-drive", "file=%s,format=raw,if=none,id=root" % root,
            "-device", "virtio-blk-pci,drive=root,disable-legacy=on,disable-modern=off",
            "-device", "pcie-root-port,id=rp1,chassis=1",
            "-drive", "file=%s,format=raw,if=none,id=pass" % images["nvme"],
            "-device", "nvme,drive=pass,serial=passthrough,bus=rp1,romfile=" + rom_path,
            "-device", "pcie-root-port,id=rp2,chassis=2",
            "-drive", "file=%s,format=raw,if=none,id=vpass" % images["virtio"],
            # iommu_platform: a virtio device of QEMU's takes no IOMMU's
            # translation unless told to -- its DMA would go to nos's own
            # memory at the guest's addresses
            "-device", "virtio-blk-pci,drive=vpass,bus=rp2,disable-legacy=on,disable-modern=off,x-pcie-flr-init=off,"
                       "iommu_platform=on"]
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
    for name in ("nvme", "virtio"):
        disk = open(images[name], "rb").read()
        pt.check("the guest's pattern is on the %s's image" % name,
                 disk[WRITE_AT:WRITE_AT + 65536] == guest_wrote(name), repr(disk[WRITE_AT:WRITE_AT + 64]))
    return finish(args, tmp, text)


def nos_checks(text):
    pt.check("nos boots", "boot: complete" in text, text[-2000:])
    pt.check("nos did not panic", "PANIC" not in text, text[text.find("PANIC"):][:2000])
    pt.check("both functions are kept from nos's drivers", "pci: %s kept for guests" % NVME_IN_NOS in text
             and "pci: %s kept for guests" % VIRTIO_IN_NOS in text, lines(text, "pci:"))
    pt.check("QEMU's unit is taken over, every device passed through", "passed through, queued invalidation" in text,
             lines(text, "vtd:"))
    pt.check("interrupts are remapped", "interrupts remapped, the IO-APIC's from" in text, lines(text, "vtd:"))
    given = re.search(r"hv: %s given to a guest .*interrupts \(MSI-X, (\d+) of its vectors\) remapped" % NVME_IN_NOS,
                      text)
    pt.check("hv gives the NVMe by its MSI-X, its interrupts remapped", given is not None, lines(text, "hv: "))
    if given:
        print("  MSI-X: %s vectors given" % given.group(1))
    # The virtio disk has no FLR: reset by its bridge as it is given and as
    # the guest lets go of it
    resets = re.findall(r"hv: %s reset by its bridge (\S+)'s secondary bus reset" % VIRTIO_IN_NOS, text)
    pt.check("the function with no FLR is reset by its bridge, given and let go of", len(resets) >= 2,
             lines(text, VIRTIO_IN_NOS))
    if resets:
        print("  %s reset by %s's secondary bus reset" % (VIRTIO_IN_NOS, resets[0]))
    pt.check("the NVMe's ROM is read and given, a copy", "hv: %s: its ROM of %d KiB given" % (NVME_IN_NOS, ROM_BYTES // 1024)
             in text, lines(text, "ROM"))


def guest_checks(text):
    pt.check("the guest boots", "pass-guest: up:" in text, text[-3000:])
    for name in ("nvme", "virtio"):
        pt.check("the guest's driver finds the %s" % name,
                 re.search(r"pass-guest: %s of [1-9]\d* sectors" % name, text) is not None, lines(text, "pass-guest:"))
        want = hashlib.md5(pattern(name)).hexdigest()
        pt.check("it reads what the runner put on the %s" % name, "pass-guest: %s read %s" % (name, want) in text,
                 lines(text, "pass-guest: %s read" % name))
        wrote = hashlib.md5(guest_wrote(name)).hexdigest()
        pt.check("it writes the %s, and reads it back past the page cache" % name,
                 "pass-guest: %s wrote %s" % (name, wrote) in text
                 and "pass-guest: %s read back %s" % (name, wrote) in text, lines(text, "pass-guest: %s" % name))
    want = "pass-guest: rom %s %d" % (hashlib.md5(rom()).hexdigest(), ROM_BYTES)
    pt.check("the guest reads the NVMe's ROM, the device's image", want in text, lines(text, "pass-guest: rom"))
    for name, word in (("nvme", "nvme"), ("virtio", "virtio")):
        rows = re.findall(r"pass-guest: irq\s+\d+:\s+((?:\d+\s+)+).*%s" % word, text)
        counts = [sum(int(n) for n in row.split()) for row in rows]
        least = 2 if name == "nvme" else 1
        pt.check("the %s's MSI-X interrupts come, on %s vector%s" % (name, "more than one" if least > 1 else "its",
                                                                    "s" if least > 1 else ""),
                 len([c for c in counts if c > 0]) >= least, lines(text, "pass-guest: irq"))
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
