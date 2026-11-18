#!/usr/bin/env python3
"""ATA test: SATA disks as the two test servers' controllers have them --
the ASUS RS500's ICH10R in IDE mode (src/crates/drivers/block/ata) as QEMU's
PIIX3 gives it on `pc`, four positions on two channels, a master and a slave
each; and the Supermicro's C236 in AHCI mode (src/crates/drivers/block/ahci)
as QEMU's q35 ICH9 gives it, a disk to a port. Both drive their disks one
command at a time, polled.

What a smoke boot cannot tell is that a command lands on the device and the
sector it was meant for. A wrong device bit or port writes the disk beside
the one asked for; a lost high byte of a 48-bit address writes 2 TiB lower;
a batch's pieces put in one command at the wrong place in its buffer
scramble a file; a recovery that does not recover leaves the disk dead
behind a failure that looked like one sector's. All of them look like a
working disk to everything above. So, four disks:

- the first (hda, ada) is a 3 TiB disk -- past 2^32 sectors, as the
  servers' 4 TB drives are -- that says it spins at 7200 rpm and has 4 KiB
  physical sectors behind 512-byte logical ones. The host marks sectors on
  both sides of 2^28 and 2^32 and the last; the kernel has to read each back
  as marked, then writes three more, high and low, and the host finds those
  three changed, their neighbours and the marks not, and nothing else on the
  disk written at all. One sector of it QEMU fails both ways (blkdebug,
  EIO): its read and its write fail, said with the device's status, and the
  sectors beside it read -- the driver recovers, and the disk is not given up
  on;
- the second (hdb, adb), an SSD: formatted ext4 by the kernel's `format`,
  mounted, a file of 6 MiB fetched onto it over HTTP and copied, mounted
  again and both read back to the host's CRC -- ext4's batches, which the
  AHCI driver makes one command a run -- small files written, unmounted;
  then held to e2fsck on the host, the big file and its copy dumped and held
  to the one served;
- the third (hdc, adc) is the CD the test boots from: a packet device, which
  the driver has to pass over;
- the fourth (hdd, add) is a GPT disk, as the servers' Ubuntu disks are: its
  partitions found, and not one byte of it changed.

At power-off every ATA disk is flushed and put in standby, and says so.

    make nos-test.iso TEST_ISO_ARGS=udpshell=9100
    tests/ata-test.py [--controller ide|ahci] [--iso nos-test.iso] [--keep]

x86-64 only. Exit code 0 = every check passed.
"""

import argparse
import functools
import hashlib
import http.server
import importlib.util
import os
import re
import subprocess
import sys
import tempfile
import threading
import time
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

spec = importlib.util.spec_from_file_location("pt", os.path.join(HERE, "parttest.py"))
pt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pt)

SECTOR = 512
BIG_BYTES = 3 << 40
BIG_SECTORS = BIG_BYTES // SECTOR
SSD_BYTES = 256 << 20
# Sectors the host marks before the boot: both sides of where a 28-bit and
# a 32-bit address run out, and the last
MARKED = [0, 1, (1 << 28) - 1, 1 << 28, (1 << 32) - 1, 1 << 32, (1 << 32) + 1, BIG_SECTORS - 1]
# Sectors the kernel writes: past 2^32, the one before the last, and a low
# one past 2^28 -- each between two sectors the host can tell from it
WRITTEN = [(1 << 32) + 5, BIG_SECTORS - 2, (1 << 28) + 7]
# The sector QEMU fails, reads and writes: past 2^32, away from every sector
# marked or written
BAD = (1 << 32) + 4096
# What a host filesystem says has data, it says in blocks of its own -- 16
# KiB on APFS -- so a sector is let off in the window of this many bytes
# around it: a command that went 2 TiB or one sector astray is still far
# outside every window
WINDOW = 64 << 10
# The file fetched onto the SSD's ext4: many batches of its blocks, and a
# last block it fills only part of
BIG_FILE_BYTES = (6 << 20) + 12345

BIG_MODEL = "TOSHIBA MG04ACA400E"
SSD_MODEL = "PNY CS900 480GB SSD"

# Each controller's machine, its four places, the names the kernel gives the
# disks at them, and what its log says of each
LAYOUTS = {
    "ide": {
        "driver": "ata",
        "machine": "pc",
        "places": ("bus=ide.0,unit=0", "bus=ide.0,unit=1", "bus=ide.1,unit=0", "bus=ide.1,unit=1"),
        "names": ("hda", "hdb", "hdc", "hdd"),
        "big": "ata: hda at ", "big_where": "channel 0 master: ",
        "ssd_where": "channel 0 slave: ",
        "cd": "channel 1 master: no ATA disk answers IDENTIFY",
        "gpt_where": "channel 1 slave: ",
    },
    "ahci": {
        "driver": "ahci",
        "machine": "q35",
        "places": ("bus=ide.0", "bus=ide.1", "bus=ide.2", "bus=ide.3"),
        "names": ("ada", "adb", "adc", "add"),
        "big": "ahci: ada at ", "big_where": "port 0: ",
        "ssd_where": "port 1: ",
        "cd": "port 2: a packet device",
        "gpt_where": "port 3: ",
    },
}

# A failed command's line, as either driver says it: the device's status
# with ERR set (an odd number), and its error register
STATUS_SAID = re.compile(r"status 0x[0-9a-f]*[13579bdf] error 0x[0-9a-f]+")

# blkdebug's rules: an EIO for any read or write that covers BAD
BLKDEBUG_CONF = """[inject-error]
event = "read_aio"
errno = "5"
sector = "%d"
once = "off"

[inject-error]
event = "write_aio"
errno = "5"
sector = "%d"
once = "off"
""" % (BAD, BAD)


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


def paths(tmp):
    return {n: os.path.join(tmp, f) for n, f in (("big", "big.raw"), ("ssd", "ssd.raw"), ("gpt", "gpt.raw"),
                                                  ("conf", "blkdebug.conf"), ("www", "www"))}


class Quiet(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass


def serve(root):
    """An HTTP server on the host's loopback for `root` -- what the guest
    reaches as 10.0.2.2 -- and its port."""
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(Quiet, directory=root))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, server.server_address[1]


def qemu(args, layout, files, log):
    big_at, ssd_at, cd_at, gpt_at = layout["places"]
    return ["qemu-system-x86_64", "-machine", layout["machine"], "-display", "none",
            "-cpu", "Westmere,-x2apic,-tsc-deadline,+pdpe1gb", "-smp", "4", "-m", "6G",
            "-serial", "file:" + log,
            # The 3 TiB disk through blkdebug, whose EIO the device reports
            # to the guest rather than stopping it
            "-drive", "if=none,id=d0,format=raw,rerror=report,werror=report,file.driver=blkdebug,"
                      "file.config=%s,file.image.driver=file,file.image.filename=%s" % (files["conf"], files["big"]),
            "-device", "ide-hd,drive=d0,%s,rotation_rate=7200,physical_block_size=4096,"
                       "logical_block_size=512,model=%s,serial=NOSATAHDD0" % (big_at, BIG_MODEL),
            "-drive", "if=none,id=d1,format=raw,file=" + files["ssd"],
            "-device", "ide-hd,drive=d1,%s,rotation_rate=1,model=%s,serial=NOSATASSD1" % (ssd_at, SSD_MODEL),
            "-drive", "if=none,id=cd,format=raw,media=cdrom,readonly=on,file=" + args.iso,
            "-device", "ide-cd,drive=cd,%s,bootindex=0" % cd_at,
            "-drive", "if=none,id=d3,format=raw,file=" + files["gpt"],
            "-device", "ide-hd,drive=d3,%s,rotation_rate=1,model=%s,serial=NOSATASSD3" % (gpt_at, SSD_MODEL),
            # A filesystem's UUID needs the random pool, and under TCG nothing
            # else seeds it: no RDRAND on this CPU, a cycle counter too even
            # for the jitter collector
            "-device", "virtio-rng-pci",
            "-device", "virtio-net-pci,netdev=n0",
            "-netdev", "user,id=n0,hostfwd=udp::%d-:%d" % (pt.SHELL_PORT, pt.SHELL_PORT)]


def log_checks(layout, text):
    big, ssd, _cd, gpt_disk = layout["names"]
    pt.check("%s is 3 TiB past 2^32 sectors, 512e at 7200 rpm" % big,
             layout["big"] in text and layout["big_where"] + BIG_MODEL in text
             and "%d sectors of 512 bytes (physical 4096), 7200 rpm" % BIG_SECTORS in text,
             "\n".join(l for l in text.splitlines() if "ata:" in l or "ahci:" in l))
    pt.check("%s is an SSD" % ssd, layout["ssd_where"] + SSD_MODEL in text and "solid state" in text)
    pt.check("the CD is passed over", layout["cd"] in text)
    pt.check("%s is the GPT disk" % gpt_disk, layout["gpt_where"] + SSD_MODEL in text)
    parts = [m for m in pt.PART_LINE.finditer(text) if m.group(1).startswith(gpt_disk)]
    _mbr, gpt = pt.expected_parts()
    found = sorted((int(m.group(2)), int(m.group(3))) for m in parts)
    pt.check("%s's GPT partitions are found" % gpt_disk, found == sorted(gpt), "%s, expected %s" % (found, gpt))


def fault_checks(layout, sh):
    """The sector QEMU fails: its read and its write fail, and the disk is
    brought back for the sectors beside it."""
    big = layout["names"][0]
    out = sh.run("diskread %s %d" % (big, BAD))
    pt.check("%s fails the read of the sector QEMU fails" % big, "read error" in out, out)
    for near in (BAD - 1, BAD + 1):
        got = read_sector(sh, big, near)
        pt.check("and then reads sector %d beside it" % near, got == bytes(SECTOR), got[:48])
    out = sh.run("diskwrite %s %d %s" % (big, BAD, written(BAD).hex()))
    pt.check("and fails a write of it", "write error" in out, out)


def fault_log_checks(layout, sh):
    """What the kernel's log says of the sector QEMU fails -- a command's
    lines go to the log, not to the serial port, so the shell's `dmesg` --
    and that no disk was given up on for it."""
    driver, big = layout["driver"], layout["names"][0]
    said = [l for l in sh.run("dmesg 400 %d" % BAD).splitlines() if "%s: %s (" % (driver, big) in l]
    pt.check("each failure is said with the device's status and error",
             len(said) >= 2 and all(STATUS_SAID.search(l) for l in said), "\n".join(said[-4:]))
    given_up = [l for l in sh.run("dmesg 4000 refused").splitlines() if "the disk is failed" in l]
    pt.check("and no disk is given up on", not given_up, "\n".join(given_up))


def shell_checks(layout, sh, port, crc):
    big, ssd, _cd, _gpt = layout["names"]
    disks = sh.run("disks")
    pt.check("disks names each disk by its model and serial",
             "disk:ata-TOSHIBA_MG04ACA400E_NOSATAHDD0" in disks and "disk:ata-PNY_CS900_480GB_SSD_NOSATASSD1" in disks,
             disks[-600:])

    fault_checks(layout, sh)

    for lba in MARKED:
        got = read_sector(sh, big, lba)
        pt.check("%s reads sector %d as the host marked it" % (big, lba), got == mark(lba), got[:48])

    for lba in WRITTEN:
        out = sh.run("diskwrite %s %d %s" % (big, lba, written(lba).hex()))
        pt.check("%s takes a write at sector %d" % (big, lba), "wrote" in out, out)
        got = read_sector(sh, big, lba)
        want = written(lba) + bytes(SECTOR - len(written(lba)))
        pt.check("and reads it back", got == want, got[:48])

    out = sh.run("format ext4 %s label=atatest" % ssd)
    pt.check("%s takes an ext4 of the kernel's own" % ssd, "formatted %s as" % ssd in out, out)
    sh.run("mkdir /ata")
    out = sh.run("mount ext4 %s /ata" % ssd)
    mounts = sh.run("mounts")
    pt.check("and mounts it", "/ata" in mounts, out + mounts)

    out = sh.run("wget -o /ata/big.bin http://10.0.2.2:%d/big.bin" % port)
    out += sh.run("cp /ata/big.bin /ata/copy.bin")
    out += sh.run("umount /ata")
    out += sh.run("mount ext4 %s /ata" % ssd)
    want = "crc32 0x%08X, %d bytes" % (crc, BIG_FILE_BYTES)
    for name in ("big.bin", "copy.bin"):
        got = sh.run("crc32 /ata/%s" % name)
        pt.check("a %d-byte file %s there reads back, mounted again, as it was served" %
                 (BIG_FILE_BYTES, "fetched" if name == "big.bin" else "copied"), want in got, out + got)

    text = "polled-from-the-sata-driver"
    sh.run("write /ata/hello.txt %s" % text)
    sh.run("mkdir /ata/dir")
    for i in range(20):
        sh.run("write /ata/dir/file%02d.txt %s-%d" % (i, text, i))
    out = sh.run("cat /ata/hello.txt")
    pt.check("a file written there reads back", text in out, out)
    out = sh.run("umount /ata")
    pt.check("and it unmounts", "error" not in out.lower() and "fail" not in out.lower(), out)
    return text


def standby_checks(layout, text):
    driver = layout["driver"]
    big, ssd, _cd, gpt = layout["names"]
    for name in (big, ssd, gpt):
        pt.check("%s is put in standby for the power-off" % name,
                 "%s: %s: in standby for the power-off" % (driver, name) in text,
                 "\n".join(l for l in text.splitlines() if "standby" in l))


def host_checks(tmp, files, gpt_before, text, served):
    big = files["big"]

    for lba in MARKED:
        pt.check("sector %d still holds its mark" % lba, get(big, lba) == mark(lba))
    for lba in WRITTEN:
        want = written(lba) + bytes(SECTOR - len(written(lba)))
        pt.check("sector %d holds what the kernel wrote" % lba, get(big, lba) == want, get(big, lba)[:48])
        for near in (lba - 1, lba + 1):
            if near not in MARKED:
                pt.check("and sector %d beside it is untouched" % near, get(big, near) == bytes(SECTOR))
    pt.check("the sector QEMU failed was not written", get(big, BAD) == bytes(SECTOR), get(big, BAD)[:48])

    extents = data_extents(big)
    if extents is None:
        print("  (no SEEK_DATA here: the whole-disk scan is skipped)")
    else:
        allowed = {(lba * SECTOR) // WINDOW for lba in MARKED + WRITTEN}
        stray = [(s, e) for s, e in extents
                 if any(w not in allowed for w in range(s // WINDOW, (e + WINDOW - 1) // WINDOW))]
        pt.check("nothing else of the 3 TiB was written", not stray, str(stray[:8]))

    pt.check("the GPT disk beside it is not one byte different", sha256(files["gpt"]) == gpt_before)

    fsck = subprocess.run(["docker", "run", "--platform", "linux/amd64", "--rm", "-v", tmp + ":/t", "nos-builder",
                           "bash", "-c", "e2fsck -fn /t/ssd.raw && debugfs -R 'cat /hello.txt' /t/ssd.raw "
                           "&& debugfs -R 'ls -l /dir' /t/ssd.raw && debugfs -R 'dump /big.bin /t/big.out' /t/ssd.raw "
                           "&& debugfs -R 'dump /copy.bin /t/copy.out' /t/ssd.raw"],
                          capture_output=True, text=True)
    pt.check("e2fsck finds the SSD's ext4 clean", fsck.returncode == 0, (fsck.stdout + fsck.stderr)[-1500:])
    pt.check("with the file in it", text is not None and text in fsck.stdout, fsck.stdout[-600:])
    pt.check("and the directory's twenty", "file19.txt" in fsck.stdout, fsck.stdout[-600:])
    for name in ("big.out", "copy.out"):
        out = os.path.join(tmp, name)
        pt.check("and the %s file, byte for byte" % ("fetched" if name == "big.out" else "copied"),
                 os.path.exists(out) and sha256(out) == served)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--controller", choices=sorted(LAYOUTS), default="ide")
    ap.add_argument("--iso", default=os.path.join(ROOT, "nos-test.iso"),
                    help="an ISO whose command line has udpshell=%d" % pt.SHELL_PORT)
    ap.add_argument("--keep", action="store_true", help="leave the images and the log behind")
    args = ap.parse_args()
    if not os.path.exists(args.iso):
        sys.exit("no %s -- make nos-test.iso TEST_ISO_ARGS=udpshell=%d" % (args.iso, pt.SHELL_PORT))
    layout = LAYOUTS[args.controller]

    tmp = tempfile.mkdtemp(prefix="nos-atatest-")
    log = os.path.join(tmp, "serial.log")
    files = paths(tmp)
    pt.sparse(files["big"], BIG_BYTES)
    for lba in MARKED:
        put(files["big"], lba, mark(lba))
    with open(files["conf"], "w") as f:
        f.write(BLKDEBUG_CONF)
    pt.sparse(files["ssd"], SSD_BYTES)
    pt.gpt_image(files["gpt"])
    gpt_before = sha256(files["gpt"])
    os.mkdir(files["www"])
    served_path = os.path.join(files["www"], "big.bin")
    with open(served_path, "wb") as f:
        f.write(os.urandom(BIG_FILE_BYTES))
    with open(served_path, "rb") as f:
        crc = zlib.crc32(f.read()) & 0xFFFFFFFF
    served = sha256(served_path)
    server, port = serve(files["www"])

    p = subprocess.Popen(qemu(args, layout, files, log), cwd=ROOT, stdout=subprocess.DEVNULL,
                         stderr=subprocess.STDOUT)
    text = None
    try:
        if not pt.check("boots with its disks on %s" % args.controller, pt.wait_log(log, "boot: complete", 900)):
            return 1
        log_checks(layout, open(log, errors="replace").read())
        sh = pt.Shell(timeout=120)
        text = shell_checks(layout, sh, port, crc)
        fault_log_checks(layout, sh)
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
        pt.check("the machine powers itself off, its disks flushed", off)
        standby_checks(layout, open(log, errors="replace").read())
    finally:
        pt.kill(p)
        server.shutdown()

    host_checks(tmp, files, gpt_before, text, served)

    if args.keep:
        print("images and log in " + tmp)
    else:
        subprocess.run(["rm", "-rf", tmp])

    print()
    if pt.failures:
        print("FAILED: " + ", ".join(pt.failures))
        return 1
    print("ata-test (%s): OK" % args.controller)
    return 0


if __name__ == "__main__":
    sys.exit(main())
