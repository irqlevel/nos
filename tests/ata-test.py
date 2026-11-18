#!/usr/bin/env python3
"""ATA test: SATA in IDE mode -- the ASUS RS500's ICH10R as its BIOS left it
-- as QEMU's PIIX3 gives it on `pc`: four positions on two channels, a master
and a slave each, the disks behind them driven by PIO and polled
(src/crates/drivers/block/ata).

What a smoke boot cannot tell is that a command lands on the device and the
sector it was meant for. A wrong device bit writes the disk beside the one
asked for; a lost high byte of a 48-bit address writes 2 TiB lower; both
look like a working disk to everything above. So, laid out as the server's
are:

- hda, the primary master, is a 3 TiB disk -- past 2^32 sectors, as the
  server's 4 TB drives are -- that says it spins at 7200 rpm and has 4 KiB
  physical sectors behind 512-byte logical ones. The host marks sectors on
  both sides of 2^28 and 2^32 and the last; the kernel has to read each back
  as marked, then writes three more, high and low, and the host finds those
  three changed, their neighbours and the marks not, and nothing else on the
  disk written at all;
- hdb, the primary slave, an SSD: formatted ext4 by the kernel's `format`,
  mounted, written and unmounted, then held to e2fsck on the host;
- hdc, the secondary master, is the CD the test boots from: a packet device,
  which the driver has to pass over;
- hdd, the secondary slave, is a GPT disk, as the server's Ubuntu disk is:
  its partitions found, and not one byte of it changed.

    make nos-test.iso TEST_ISO_ARGS=udpshell=9100
    tests/ata-test.py [--iso nos-test.iso] [--keep]

x86-64 only. Exit code 0 = every check passed.
"""

import argparse
import hashlib
import importlib.util
import os
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

spec = importlib.util.spec_from_file_location("pt", os.path.join(HERE, "parttest.py"))
pt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pt)

SECTOR = 512
HDA_BYTES = 3 << 40
HDA_SECTORS = HDA_BYTES // SECTOR
HDB_BYTES = 256 << 20
# Sectors the host marks before the boot: both sides of where a 28-bit and
# a 32-bit address run out, and the last
MARKED = [0, 1, (1 << 28) - 1, 1 << 28, (1 << 32) - 1, 1 << 32, (1 << 32) + 1, HDA_SECTORS - 1]
# Sectors the kernel writes: past 2^32, the one before the last, and a low
# one past 2^28 -- each between two sectors the host can tell from it
WRITTEN = [(1 << 32) + 5, HDA_SECTORS - 2, (1 << 28) + 7]
# What a host filesystem says has data, it says in blocks of its own -- 16
# KiB on APFS -- so a sector is let off in the window of this many bytes
# around it: a command that went 2 TiB or one sector astray is still far
# outside every window
WINDOW = 64 << 10

HDA_MODEL = "TOSHIBA MG04ACA400E"
SSD_MODEL = "PNY CS900 480GB SSD"


def mark(lba):
    """What the host puts in a marked sector: its own number, over and over."""
    text = b"nos-ata-test lba %020d " % lba
    return (text * (SECTOR // len(text) + 1))[:SECTOR]


def written(lba):
    """What the kernel writes to one of WRITTEN: a pattern of its own, the
    rest of the sector zeros (diskwrite)."""
    return (b"written by nos %020d " % lba) * 2


def put(path, lba, data):
    with open(path, "r+b") as f:
        f.seek(lba * SECTOR)
        f.write(data)


def get(path, lba):
    with open(path, "rb") as f:
        f.seek(lba * SECTOR)
        return f.read(SECTOR)


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def data_extents(path):
    """Where a sparse file has data: (start, end) byte ranges, by SEEK_DATA
    and SEEK_HOLE. None where the filesystem says nothing of its holes."""
    if not hasattr(os, "SEEK_DATA"):
        return None
    out = []
    fd = os.open(path, os.O_RDONLY)
    try:
        size = os.fstat(fd).st_size
        pos = 0
        while pos < size:
            try:
                start = os.lseek(fd, pos, os.SEEK_DATA)
            except OSError:
                break
            end = os.lseek(fd, start, os.SEEK_HOLE)
            out.append((start, end))
            pos = end
    finally:
        os.close(fd)
    return out


def read_sector(sh, disk, lba):
    """A sector as `diskread` shows it, back into bytes: a byte a token, in
    hex, a zero as `0`."""
    rows = pt.dump_bytes(sh.run("diskread %s %d" % (disk, lba)))
    try:
        return bytes(int(t, 16) for row in rows for t in row)
    except ValueError:
        return b""


def qemu(args, tmp, log):
    hda, hdb, hdd = (os.path.join(tmp, n) for n in ("hda.raw", "hdb.raw", "hdd.raw"))
    return ["qemu-system-x86_64", "-machine", "pc", "-display", "none",
            "-cpu", "Westmere,-x2apic,-tsc-deadline,+pdpe1gb", "-smp", "4", "-m", "6G",
            "-serial", "file:" + log,
            "-drive", "if=none,id=d0,format=raw,file=" + hda,
            "-device", "ide-hd,drive=d0,bus=ide.0,unit=0,rotation_rate=7200,physical_block_size=4096,"
                       "logical_block_size=512,model=%s,serial=NOSATAHDD0" % HDA_MODEL,
            "-drive", "if=none,id=d1,format=raw,file=" + hdb,
            "-device", "ide-hd,drive=d1,bus=ide.0,unit=1,rotation_rate=1,model=%s,serial=NOSATASSD1" % SSD_MODEL,
            "-drive", "if=none,id=cd,format=raw,media=cdrom,readonly=on,file=" + args.iso,
            "-device", "ide-cd,drive=cd,bus=ide.1,unit=0,bootindex=0",
            "-drive", "if=none,id=d3,format=raw,file=" + hdd,
            "-device", "ide-hd,drive=d3,bus=ide.1,unit=1,rotation_rate=1,model=%s,serial=NOSATASSD3" % SSD_MODEL,
            # A filesystem's UUID needs the random pool, and under TCG nothing
            # else seeds it: no RDRAND on this CPU, a cycle counter too even
            # for the jitter source. The server's real Westmere seeds it by
            # jitter alone.
            "-device", "virtio-rng-pci",
            "-device", "virtio-net-pci,netdev=n0",
            "-netdev", "user,id=n0,hostfwd=udp::%d-:%d" % (pt.SHELL_PORT, pt.SHELL_PORT)]


def log_checks(text):
    pt.check("hda is the primary master, 3 TiB past 2^32 sectors, 512e at 7200 rpm",
             "ata: hda at " in text and "channel 0 master: %s" % HDA_MODEL in text
             and "%d sectors of 512 bytes (physical 4096), 7200 rpm" % HDA_SECTORS in text,
             "\n".join(l for l in text.splitlines() if "ata:" in l))
    pt.check("hdb is the primary slave, an SSD",
             "channel 0 slave: %s" % SSD_MODEL in text and "solid state" in text)
    pt.check("the CD at the secondary master is passed over",
             "channel 1 master: no ATA disk answers IDENTIFY" in text)
    pt.check("hdd is the secondary slave", "channel 1 slave: %s" % SSD_MODEL in text)
    parts = [m for m in pt.PART_LINE.finditer(text) if m.group(1).startswith("hdd")]
    _mbr, gpt = pt.expected_parts()
    found = sorted((int(m.group(2)), int(m.group(3))) for m in parts)
    pt.check("hdd's GPT partitions are found", found == sorted(gpt), "%s, expected %s" % (found, gpt))


def shell_checks(sh, tmp):
    disks = sh.run("disks")
    pt.check("disks names each ATA disk by its model and serial",
             "disk:ata-TOSHIBA_MG04ACA400E_NOSATAHDD0" in disks and "disk:ata-PNY_CS900_480GB_SSD_NOSATASSD1" in disks,
             disks[-600:])

    for lba in MARKED:
        got = read_sector(sh, "hda", lba)
        pt.check("hda reads sector %d as the host marked it" % lba, got == mark(lba), got[:48])

    for lba in WRITTEN:
        out = sh.run("diskwrite hda %d %s" % (lba, written(lba).hex()))
        pt.check("hda takes a write at sector %d" % lba, "wrote" in out, out)
        got = read_sector(sh, "hda", lba)
        want = written(lba) + bytes(SECTOR - len(written(lba)))
        pt.check("and reads it back", got == want, got[:48])

    out = sh.run("format ext4 hdb label=atatest")
    pt.check("hdb takes an ext4 of the kernel's own", "formatted hdb as" in out, out)
    sh.run("mkdir /ata")
    out = sh.run("mount ext4 hdb /ata")
    mounts = sh.run("mounts")
    pt.check("and mounts it", "/ata" in mounts, out + mounts)
    text = "polled-pio-from-the-ata-driver"
    sh.run("write /ata/hello.txt %s" % text)
    sh.run("mkdir /ata/dir")
    for i in range(20):
        sh.run("write /ata/dir/file%02d.txt %s-%d" % (i, text, i))
    out = sh.run("cat /ata/hello.txt")
    pt.check("a file written there reads back", text in out, out)
    out = sh.run("umount /ata")
    pt.check("and it unmounts", "error" not in out.lower() and "fail" not in out.lower(), out)
    return text


def host_checks(tmp, hdd_before, text):
    hda, hdb, hdd = (os.path.join(tmp, n) for n in ("hda.raw", "hdb.raw", "hdd.raw"))

    for lba in MARKED:
        pt.check("sector %d still holds its mark" % lba, get(hda, lba) == mark(lba))
    for lba in WRITTEN:
        want = written(lba) + bytes(SECTOR - len(written(lba)))
        pt.check("sector %d holds what the kernel wrote" % lba, get(hda, lba) == want, get(hda, lba)[:48])
        for near in (lba - 1, lba + 1):
            if near not in MARKED:
                pt.check("and sector %d beside it is untouched" % near, get(hda, near) == bytes(SECTOR))

    extents = data_extents(hda)
    if extents is None:
        print("  (no SEEK_DATA here: the whole-disk scan is skipped)")
    else:
        allowed = {(lba * SECTOR) // WINDOW for lba in MARKED + WRITTEN}
        stray = [(s, e) for s, e in extents
                 if any(w not in allowed for w in range(s // WINDOW, (e + WINDOW - 1) // WINDOW))]
        pt.check("nothing else of the 3 TiB was written", not stray, str(stray[:8]))

    pt.check("hdd, the GPT disk beside it, is not one byte different", sha256(hdd) == hdd_before)

    fsck = subprocess.run(["docker", "run", "--platform", "linux/amd64", "--rm", "-v", tmp + ":/t", "nos-builder",
                           "bash", "-c", "e2fsck -fn /t/hdb.raw && debugfs -R 'cat /hello.txt' /t/hdb.raw "
                           "&& debugfs -R 'ls -l /dir' /t/hdb.raw"],
                          capture_output=True, text=True)
    pt.check("e2fsck finds hdb's ext4 clean", fsck.returncode == 0, (fsck.stdout + fsck.stderr)[-1500:])
    pt.check("with the file in it", text in fsck.stdout, fsck.stdout[-600:])
    pt.check("and the directory's twenty", "file19.txt" in fsck.stdout, fsck.stdout[-600:])


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--iso", default=os.path.join(ROOT, "nos-test.iso"),
                    help="an ISO whose command line has udpshell=%d" % pt.SHELL_PORT)
    ap.add_argument("--keep", action="store_true", help="leave the images and the log behind")
    args = ap.parse_args()
    if not os.path.exists(args.iso):
        sys.exit("no %s -- make nos-test.iso TEST_ISO_ARGS=udpshell=%d" % (args.iso, pt.SHELL_PORT))

    tmp = tempfile.mkdtemp(prefix="nos-atatest-")
    log = os.path.join(tmp, "serial.log")
    hda, hdb, hdd = (os.path.join(tmp, n) for n in ("hda.raw", "hdb.raw", "hdd.raw"))
    pt.sparse(hda, HDA_BYTES)
    for lba in MARKED:
        put(hda, lba, mark(lba))
    pt.sparse(hdb, HDB_BYTES)
    pt.gpt_image(hdd)
    hdd_before = sha256(hdd)

    p = subprocess.Popen(qemu(args, tmp, log), cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)
    text = None
    try:
        if not pt.check("boots with its disks on IDE", pt.wait_log(log, "boot: complete", 900)):
            return 1
        log_checks(open(log, errors="replace").read())
        sh = pt.Shell(timeout=120)
        text = shell_checks(sh, tmp)
        try:
            sh.sock.settimeout(5)
            sh.run("poweroff")
        except Exception:
            pass
        off = False
        for _ in range(120):
            if p.poll() is not None:
                off = True
                break
            time.sleep(1)
        pt.check("the machine powers itself off, its disks flushed", off and "ata:" in open(log, errors="replace").read())
    finally:
        pt.kill(p)

    host_checks(tmp, hdd_before, text)

    if args.keep:
        print("images and log in " + tmp)
    else:
        subprocess.run(["rm", "-rf", tmp])

    print()
    if pt.failures:
        print("FAILED: " + ", ".join(pt.failures))
        return 1
    print("ata-test: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
