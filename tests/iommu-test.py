#!/usr/bin/env python3
"""VT-d (arch/x86_64/vtd.cpp): nos taking over QEMU's Intel IOMMU with
iommu=on, every device passed through, and a DMA the unit blocks said.

QEMU's q35 with -device intel-iommu: its DMAR names one unit for every
device, which nos takes over -- root and context tables, queued
invalidation, the fault event at the boot CPU. The NVMe's DMA goes through
it; virtio's goes around, as QEMU's virtio devices bypass the unit unless
told iommu_platform=on. One boot:

- the unit is taken: the boot log says it translates, `iommu` says on;
- the NVMe is read through it: sector 0, the image's bytes;
- `iommu identity` moves the NVMe to a domain of its own that maps every
  page of RAM to itself: it reads the same through the domain's
  second-level table, and again once `iommu release` passes it through;
- `iommu block` makes the NVMe's context entry not present: a read of it
  is then a DMA the unit blocks -- the command's fetch -- said with the
  NVMe's requester id and the reason, no context entry, and the machine
  goes on: the console's shell answers, and `iommu` counts the fault;
- `iommu unblock` passes it through again;
- and all of it with interrupts remapped (`intremap=on`, unless
  --no-intremap): the boot log says the unit took its interrupt table and
  the IO-APIC's requester id, `iommu` says interrupts are remapped and how
  many entries its table holds, the IO-APIC's among them -- the NVMe's
  MSI-X, virtio-net's and the timer's all through it, or nothing above
  would be answered.

The read itself is not answered: QEMU's NVMe takes the blocked fetch for a
fatal error of its own (CSTS.CFS), and nos's NVMe driver waits for the
command with no deadline -- a gap of the driver's, not this gate's. So the
read is left to the UDP shell, and what follows is asked of the console's
shell, on the serial port; and lines logged while a UDP shell's command
runs reach dmesg and not the serial port, so the fault is read by `dmesg`.

    make nos-test.iso TEST_ISO_ARGS="iommu=on udpshell=9100"
    tests/iommu-test.py [--iso nos-test.iso] [--keep]

x86-64 only. Exit code 0 = every check passed.
"""

import argparse
import importlib.util
import os
import re
import socket
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

spec = importlib.util.spec_from_file_location("pt", os.path.join(HERE, "parttest.py"))
pt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pt)

SECTOR = 512
PATTERN = b"nos iommu test, sector zero. " * 18
TAKEN = "passed through, queued invalidation"
REMAPPED = "interrupts remapped, the IO-APIC's from"
SERIAL_PORT = 4555


class Console:
    """The console's shell over QEMU's serial port, a TCP socket whose
    output QEMU also writes to the log: a command typed, and what the log
    gained until the next prompt."""

    def __init__(self, log):
        self.log = log
        self.sock = socket.create_connection(("127.0.0.1", SERIAL_PORT), timeout=10)

    def run(self, cmd, timeout=30):
        start = len(open(self.log, "rb").read())
        self.sock.sendall((cmd + "\r").encode())
        deadline = time.time() + timeout
        out = ""
        while time.time() < deadline:
            time.sleep(0.5)
            out = open(self.log, "rb").read()[start:].decode(errors="replace")
            body = out.split(cmd, 1)[-1]
            if "\n$" in body or body.rstrip().endswith("$"):
                break
        print("  console$ " + cmd)
        for line in out.split(cmd, 1)[-1].splitlines():
            print("    " + line)
        return out.split(cmd, 1)[-1]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--iso", default=os.path.join(ROOT, "nos-test.iso"),
                    help="an ISO whose command line has iommu=on udpshell=%d" % pt.SHELL_PORT)
    ap.add_argument("--keep", action="store_true", help="leave the image and the log behind")
    ap.add_argument("--no-intremap", action="store_true",
                    help="QEMU's unit without interrupt remapping: nos's interrupts left in their own format")
    args = ap.parse_args()
    if not os.path.exists(args.iso):
        sys.exit("no %s -- make nos-test.iso TEST_ISO_ARGS=\"iommu=on udpshell=%d\"" % (args.iso, pt.SHELL_PORT))

    tmp = tempfile.mkdtemp(prefix="nos-iommutest-")
    image = os.path.join(tmp, "nvme.img")
    with open(image, "wb") as f:
        f.write(PATTERN[:SECTOR])
        f.truncate(1 << 20)
    log = os.path.join(tmp, "nos.log")

    # intremap's default is QEMU's to choose -- auto, which is on under TCG
    # since 10.x -- so the gate says which it means
    unit = "intel-iommu,intremap=off" if args.no_intremap else "intel-iommu,intremap=on"
    argv = ["qemu-system-x86_64", "-machine", "q35", "-device", unit,
            "-display", "none", "-smp", "2", "-m", "1G",
            "-chardev", "socket,id=ser0,host=127.0.0.1,port=%d,server=on,wait=off,logfile=%s" % (SERIAL_PORT, log),
            "-serial", "chardev:ser0",
            "-cdrom", args.iso,
            "-drive", "if=none,id=nvme0,format=raw,file=" + image,
            "-device", "nvme,serial=iommu0,drive=nvme0",
            "-device", "virtio-net-pci,netdev=n0",
            "-netdev", "user,id=n0,hostfwd=udp::%d-:%d" % (pt.SHELL_PORT, pt.SHELL_PORT)]
    p = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)
    try:
        if not pt.check("boots to the shell", pt.wait_log(log, "boot: complete", 600)):
            return finish(args, tmp, log)
        text = open(log, errors="replace").read()
        pt.check("the unit is taken over, every device passed through", TAKEN in text,
                 "\n".join(l for l in text.splitlines() if "vtd:" in l))
        found = re.search(r"found NVMe device \S+ at ([0-9a-f]{2}:[0-9a-f]{2}\.[0-7])", text)
        if not pt.check("the NVMe is found", found is not None):
            return finish(args, tmp, log)
        bdf = found.group(1)

        if args.no_intremap:
            pt.check("interrupts are not remapped", REMAPPED not in text,
                     "\n".join(l for l in text.splitlines() if "vtd:" in l))
        else:
            pt.check("the unit remaps interrupts, the IO-APIC's verified", REMAPPED in text,
                     "\n".join(l for l in text.splitlines() if "vtd:" in l))

        sh = pt.Shell(timeout=90)
        state = sh.run("iommu")
        pt.check("iommu says on", "iommu: on" in state and "translating" in state, state)
        if not args.no_intremap:
            entries = re.search(r"interrupts remapped: (\d+) of \d+ entries, the IO-APIC's among them", state)
            pt.check("iommu says interrupts are remapped, entries held for the IO-APIC and the devices",
                     "iommu: on, interrupts remapped" in state and entries is not None and int(entries.group(1)) >= 3,
                     state)

        dump = sh.run("diskread nvme0 0")
        # diskread's rows: the offset, then each byte in hex, unpadded
        want = "0: " + " ".join("%X" % b for b in PATTERN[:16])
        pt.check("the NVMe reads through the unit", want in dump, dump[:400])

        # A domain of its own, every page of RAM mapped to itself: the same
        # addresses, through a second-level table -- 2 MiB leaves and 4 KiB
        # ones about the firmware's holes
        moved = try_run(sh, "iommu identity " + bdf)
        pt.check("iommu identity moves the NVMe to a domain of its own", "in a domain of its own" in moved, moved)
        dump = try_run(sh, "diskread nvme0 0")
        pt.check("the NVMe reads through its domain's tables", want in dump, dump[:400])
        back = try_run(sh, "iommu release " + bdf)
        pt.check("iommu release passes it through again", "passed through" in back, back)
        dump = try_run(sh, "diskread nvme0 0")
        pt.check("and it reads passed through again", want in dump, dump[:400])

        blocked = sh.run("iommu block " + bdf)
        pt.check("iommu block takes the NVMe off", "blocked" in blocked, blocked)

        # The read: never answered (see above), so not waited for here
        threading.Thread(target=lambda: quiet(sh, "diskread nvme0 0"), daemon=True).start()

        con = Console(log)
        said = re.compile(r"vtd: dmar\d+: (read|write) of 0x[0-9A-F]+ by %s blocked: 0x02, no context entry"
                          % re.escape(bdf))
        deadline = time.time() + 60
        found_line = ""
        while time.time() < deadline:
            time.sleep(3)
            found_line = con.run("dmesg 50 vtd")
            if said.search(found_line):
                break
        pt.check("the blocked DMA is said, with the NVMe's id and the reason", said.search(found_line) is not None,
                 found_line[-2000:])

        after = con.run("iommu")
        faults = re.search(r"faults (\d+)", after)
        pt.check("the machine goes on, the fault counted", faults is not None and int(faults.group(1)) >= 1, after)

        again = con.run("iommu unblock " + bdf)
        pt.check("iommu unblock passes it through again", "passed through" in again, again)
        pt.check("nothing panicked", "PANIC" not in open(log, errors="replace").read())
    finally:
        pt.kill(p)
    return finish(args, tmp, log)


def quiet(sh, cmd):
    """A command whose answer may never come"""
    try:
        sh.run(cmd)
    except Exception:
        pass


def try_run(sh, cmd):
    """A command's answer, or "" where none came: a DMA a domain does not let
    through leaves the NVMe driver waiting for good (see above), and the
    check that wanted the answer fails rather than the gate stopping"""
    try:
        return sh.run(cmd)
    except Exception as e:
        print("  (no answer to %s: %s)" % (cmd, e))
        return ""


def finish(args, tmp, log):
    if args.keep or pt.failures:
        print("image and log in " + tmp)
    else:
        subprocess.run(["rm", "-rf", tmp])
    print()
    if pt.failures:
        print("FAILED: " + ", ".join(pt.failures))
        return 1
    print("iommu-test: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
