#!/usr/bin/env python3
"""hv test: the hypervisor module, loaded into a running kernel, turning the
CPU's virtualization extension on and -- above all -- off again, and running
its built-in guests under it.

The extension is the one piece of state a loadable module takes that does not
belong to the module: `EFER.SVME` and `MSR_VM_HSAVE_PA` on AMD, `CR4.VMXE`
and VMX root operation on Intel, are the CPU's, and they outlive an `rmmod`
that forgets them. What is left behind then is worse than a leak: the page
the CPU was told to save host state into has been freed and handed to
somebody else, and the code that would have turned it off has been unmapped.
Nothing in the kernel would say so, and the machine would go on looking
healthy until the next load.

So every check here that matters asks the *hardware*, not the module's own
bookkeeping: `hv` reports the mask it gets by sending each CPU an IPI that
reads the register. The load-unload-load round is the test -- if the first
unload left anything on, the second load says so, and it is a failure here.

What is checked, on x86-64 (the architecture where there is an extension at
all -- `-cpu max` gives QEMU's TCG an AMD-V with nested paging, and under
KVM it is whatever the host CPU has):

  - the module loads on a machine with an extension, and says which
  - `hv info` reports nested paging, which is what a guest cannot do without
  - it is off for every CPU to begin with, and a guest is not run on a CPU
    it is off for
  - `hv on <cpu>` turns it on for that CPU and no other: a guest bound to it
    runs, a guest bound to another is not run; `hv off` undoes it
  - `hv on` turns it on for every running CPU
  - every built-in guest does what it was told on the first CPU and on the
    last: port I/O and CPUID answered by the host, long mode with memory
    above 4 GiB and every register across a hypercall, a write past its
    memory stopped at the nested table, a triple fault that stops only the
    guest, a VMCB that breaks a rule refused with the rule named, a
    `cli; jmp $` the host's interrupts get through and the host stops,
    three VMs on one CPU that each read their own page through the TLB --
    two taking turns, and a third given an ASID one of them had, after the
    flush that ended their generation (meaningful on a CPU that keeps
    translations; TCG flushes on every entry) -- and a guest of two CPUs,
    each on a host CPU of its own, the first starting the second with INIT
    and a start-up IPI into real mode, the second reaching long mode and
    sending the first an IPI, and the first's APIC timer interrupting it;
    and a guest of two CPUs taking turns on the one host CPU by IPI, each
    finding its YMM registers, MXCSR and x87 control word as it left them
    at every turn, and YMM0-15 zero as it starts, its XSETBVs held to what
    a CPU takes. Under TCG the guests of two CPUs need QEMU 9.2 or later,
    which puts a real-mode guest through the nested page table; the test
    refuses an older one
  - a second `insmod`, a CPU that does not exist, a word that is not a
    subcommand and a guest that does not exist are each refused
  - `rmmod` with the extension on for every CPU turns it off for every CPU
  - loading it again finds nothing left on -- the check the rest is for --
    and keeps the VMs' folders in /vms, given no `vms=`
  - `insmod` given a parameter hv has not got, or a `vms=` that is no
    absolute path, refuses the load and says why; given `vms=<folder>`,
    `hv` and `hv vms` say that folder
  - `hv convert` makes qcow2 images qemu-img made of a disk made here --
    compressed, and version 2 -- the disk they were made of, as its SHA-256
    has it; refuses to write over a disk, an image cut short (leaving
    nothing behind) and a disk that is raw already (on arm64 too)
  - nothing warned, and nothing panicked

and on arm64, where a hypervisor needs EL2 and this kernel dropped to EL1 in
its first hundred instructions:

  - given a parameter, the module refuses the load -- it takes none here
  - the module loads, says so, and refuses to run anything
  - `hv info` reports the exception level and what stage-2 would offer
  - `hv on` and `hv run` are refused rather than attempted
  - `hv convert`, which needs no guest, does what it does on x86-64
  - it unloads

    scripts/hv-test.py [--arch x86_64|aarch64] [--tcg] [--keep]

Exit code 0 = every check passed.
"""

import argparse
import hashlib
import importlib.util
import os
import platform
import random
import re
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

spec = importlib.util.spec_from_file_location("pt", os.path.join(HERE, "parttest.py"))
pt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pt)

SHELL_PORT = pt.SHELL_PORT
CPUS = 4
ALL_CPUS = "0-%d" % (CPUS - 1)
ALL_MASK = "0x%x" % ((1 << CPUS) - 1)

# What `hv convert` is asked, with hv loaded -- on x86-64 from the boot
# script, on arm64 one at a time -- and what each must answer: images
# qemu-img made of a disk made here (convert_images), converted on nos's own
# ext2 and held to the disk by its SHA-256; a second conversion over the
# first refused; an image cut short refused and nothing left of it; a disk
# that is raw already refused, saying so; and the usage.
CONVERT = [
    "hv convert /img.qcow2 /img.raw",
    "sha256 /img.raw",
    "hv convert /img.qcow2 /img.raw",
    "hv convert /v2.qcow2 /v2.raw",
    "sha256 /v2.raw",
    "hv convert /cut.qcow2 /cut.raw",
    "ls /",
    "hv convert /disk.raw /disk2.raw",
    "hv convert",
]
# The disk the images are of: not a whole number of clusters, its last
# sector written.
CONVERT_SIZE = 6 * 1024 * 1024 + 3 * 512


def create_script(port):
    """What `hv create <name> <url>` is asked, from the test's web server --
    which QEMU's user network puts at 10.0.2.2 for nos -- and what each must
    answer (create_checks): a qcow2 image downloaded, made a disk and grown
    to size=, given the seed and a vm.toml; the same name again refused; a
    raw image taken as it is; a URL with nothing at it, a file that is no
    image and a size= under the image's refused, each leaving nothing."""
    url = "http://10.0.2.2:%d/" % port
    return [
        "hv create c1 %simg.qcow2 size=16M mem=512 password=nos" % url,
        "ls /vms/c1",
        "sha256 /vms/c1/disk.raw",
        "cat /vms/c1/vm.toml",
        "hv create c1 %simg.qcow2" % url,
        "hv create c2 %sraw.img" % url,
        "sha256 /vms/c2/disk.raw",
        "hv create c3 %smissing.qcow2" % url,
        "hv create c4 %snotes.txt" % url,
        "hv create c5 %simg.qcow2 size=1M" % url,
        "ls /vms",
        "hv vms",
    ]


# The size hv create grows the disk to, and a raw image's.
CREATE_SIZE = 16 * 1024 * 1024
RAW_SIZE = 2 * 1024 * 1024
# The key nos's sshd lets in, which hv create gives cloud-init.
TEST_KEY = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHVub3QtYS1yZWFsLWtleS1qdXN0LWEtdGVzdA hv-test"

# One line a command, in the order /etc/rc runs them. The test indexes the
# output by position, so the two lists are one thing.
SCRIPT = [
    "insmod /hv.ko",        # 0
    "hv info",              # 1
    "hv",                   # 2
    "hv run hypercall",     # 3
    "hv on 1",              # 4
    "hv",                   # 5
    "hv run hypercall 1",   # 6
    "hv run hypercall 2",   # 7
    "hv off",               # 8
    "hv",                   # 9
    "hv on",                # 10
    "hv",                   # 11
    "hv run all 0",         # 12
    "hv run all %d" % (CPUS - 1),  # 13
    "hv",                   # 14
    "insmod /hv.ko",        # 15
    "hv on 99",             # 16
    "hv nonsense",          # 17
    "hv run nonsense",      # 18
    "rmmod hv",             # 19
    "insmod /hv.ko",        # 20
    "hv",                   # 21
    "rmmod hv",             # 22
    "insmod /hv.ko vmz=/x", # 23
    "insmod /hv.ko vms=data",  # 24
    "insmod /hv.ko vms=/data/vms/",  # 25
    "hv",                   # 26
    "hv vms",               # 27
    "rmmod hv",             # 28
    "lsmod",                # 29
    "dmesg 400 hv:",        # 30
    # A PVH image (hv/src/pvh.rs): hvboot, started at its 32-bit entry with
    # paging off -- under VT-x, by unrestricted guest -- and given no disk.
    "insmod /hv.ko",        # 31
    "hv on",                # 32
    "hv boot /hvboot mem=64 secs=120 cmdline=hvboot.verbose",  # 33
    # And of two CPUs: the MP table written beside the start info, the
    # second CPU waiting for a start-up IPI that never comes.
    "hv boot /hvboot mem=64 cpus=2 secs=120 cmdline=hvboot.verbose",  # 34
    "rmmod hv",             # 35
] + [
    # `hv convert`, a cloud's qcow2 image made a raw disk (CONVERT, above),
    # and then `hv create` of a VM from a URL (create_script, below) --
    # whose lines need the port of the test's web server -- and an rmmod.
    "insmod /hv.ko",        # 36
] + CONVERT

# hvboot, as `make` builds it: the PVH image the script boots.
HVBOOT = os.path.join(ROOT, "out", "x86_64", "hvboot")

# What `hv run all` has to have said about each guest, beyond its verdict.
GUESTS = {
    "exits": [r'said\s+"nos: ports and cpuid"', r"1 port in, 20 port out, 1 cpuid"],
    "uart": [r"brought up the 8250 and sent 35 bytes through it"],
    "hypercall": [r'said\s+"nos: long mode"', r"15 registers went out at the hypercall and 15 answers came back"],
    "fault": [r"nested page fault at gpa 0x1ff000, error 0x1[0-9a-f]{8}, rip 0x8009",
              r"the instruction it was told to, its state read whole"],
    "absent": [r"nested page fault at gpa 0xfed803c0, error 0x1[0-9a-f]{8}, rip 0x801b",
               r"both reads found all ones through one read-only page"],
    "mmio": [r"14 stores and loads of every size decoded and performed, each value where it belonged"],
    "triple": [r"shutdown \(triple fault\)"],
    "refused": [r"not entered -- the VMCB breaks a rule: CR0.NW is set without CR0.CD"],
    "spin": [r"stopped\s+by the host", r"interrupts got through [1-9]\d* times"],
    "tpr": [r"wrote 15 to its CR8 and read it back, the CPU keeping the shadow itself, and the host's CR8 is still 0"],
    "asid": [r"vm C was given ASID \d+, which vm [AB] had, after \d+ generation\(s\) ended, and read its own too"],
    "smp": [r"cpu 1 started by INIT and a start-up IPI, came up in real mode and reached long mode",
            r"its IPI reached cpu 0, whose one-shot APIC timer then ran out and interrupted it",
            r"stopped\s+both CPUs halted with interrupts off"],
    "ioapic": [r"two edges taken, none while masked; three levels, each sent again at its EOI while the line "
               r"stayed up, its remote IRR set in service and clear after",
               r"the IO-APIC sent 5, 3 of them levels, 3 ended by EOI"],
    # What the guest sees has to be the same whoever delivers its
    # interrupts; who does is said after the dash -- the host here.
    "apicv": [r"CR8 5 held 0x50 and 0x30, both read in IRR; CR8 0 let 0x50 in, then 0x30 with 0x70 nested in its "
              r"handler, ISR showing each in service; three levels from the IO-APIC, sent again at each EOI while "
              r"the line stayed up; nothing left in service -- the host injected all six",
              r"the IO-APIC sent 3, 3 of them levels, 3 ended by EOI"],
    # Who hands the second CPU's IPIs over -- the host here -- is said after
    # the colon; that the first took them all is what has to be the same.
    "posted": [r"the first CPU took all 1000 of the second's IPIs while it spun, the host handing each over"],
    "debug": [r"both #DBs its own handler's: the single step's with DR6\.BS, after the NOP; the ICEBP's with no bit",
              r"2 exceptions given back"],
    # Its reads are stopped and answered by the host -- 11 accesses
    # performed -- except on QEMU's TCG, which checks a read of CR4 against
    # CR0's intercept and so never stops one: 5, its writes. The guest's own
    # check knows which by the hypervisor's CPUID signature.
    # The host performs every access, 11 of them, or on QEMU's TCG -- which
    # stops no read of CR4 -- the 5 writes; and refuses PKE, PAE cleared and
    # bit 32. A CPU may refuse the last two itself, before its intercept, as
    # the AX41's Zen 2 does under KVM: then 9 and 1, and the guest says so.
    "cr4": [r"while the CPU's CR4 kept MCE; PKE, PAE cleared and bit 32 each a #GP, CR4 as it was",
            r"(?:it read back what it wrote, MCE cleared and then PGE set|its reads the CPU's own, QEMU's TCG)",
            r"\b(?:(?:11|5) of CR4 performed, 3 refused|9 of CR4 performed, 1 refused\b.*\n(?:.*\n)*.*"
            r"2 of them the CPU's own, before its intercept)"],
    # Every MSR comes to the policy on both backends, so this one says the
    # same under either.
    "msr": [r"SPEC_CTRL, PRED_CMD, LS_CFG and MTRRs turned on each a #GP its own handler took",
            r"CPUID offers no speculation control and no PMU, and says a hypervisor is there",
            r"\b5 rdmsr, 3 wrmsr, 4 refused"],
    # Two VMs, one after the other on the CPU the guests run on: the second
    # must read 0 where the first left its own, and each one's breakpoint,
    # on an instruction of its own, fire -- DR0-DR3 made its own before each
    # of its entries. An execution breakpoint: QEMU's TCG never gets past a
    # store a data breakpoint watches in a nested guest.
    "dr": [r"vm B read DR0-DR3 as 0 where vm A had left its own, each read back the pattern it wrote",
           r"each one's breakpoint fired at its own address: DR6\.B0, at its ret",
           r"\b14 and 14 movs of debug registers performed, 1 and 1 exceptions given back"],
    # Two CPUs taking turns on the CPU the guests run on, by IPI: each has to
    # find YMM0-15 0 as it starts, though the other's pattern was in the
    # CPU's registers, and its YMM registers, MXCSR and x87 control word as
    # it left them at every turn -- put back by the host each time the other
    # had had the CPU (`hvarch::x86::fp`); its XSETBVs held to what a CPU
    # takes, and leaf 0xD as a CPU gives it. The XSETBV with CR4.OSXSAVE
    # clear is a #UD the CPU raises itself under VT-x, and the host's under
    # QEMU's TCG, whose intercept comes first: 6 or 5 refused.
    "avx": [r"XSAVE and AVX offered(?: \(AVX2 too\))?, OSXSAVE with CR4's, XCR0 1 to 7, the six XSETBVs a CPU "
            r"refuses refused, leaf 0xD's sizes 576 and 832",
            r"YMM0-15 0 at each CPU's start, and its YMM registers, MXCSR and control word its own at all 64 of "
            r"its turns",
            r"cpu 0: \d+ \(\d XSETBV, [56] refused;.*cpu 1: \d+ \(\d XSETBV, [56] refused;"],
}

# Five of the guests test a mechanism each vendor has its own way: AMD-V's
# software VMCB check (`refused`), its ASID recycling (`asid`), its shadow
# TPR (`tpr`), its CR4 intercepts (`cr4`) and its intercept of DR0-DR3
# (`dr`). Under Intel VT-x the CPU refuses a bad VMCS itself; the ASID is a
# VPID held for the guest's life, so the third VM gets one of the first two's
# back; CR8 is the host's, so the guest is stopped at each access and
# answered from a shadow -- where the CPU has no APIC virtualization
# (`GUESTS_APICV`); a write of CR4 stops it only where it changes a bit
# the host keeps -- three of five -- its reads the read shadow's, and PAE
# cleared the CPU's own #GP; and every `mov` of a debug register stops it,
# DR6's and DR7's in its #DB handler too -- eighteen. What each says differs;
# the rest is one, `debug` among it: a #DB given back with the DR6 the exit
# said.
GUESTS_VMX = {
    "refused": [r"the CPU refused the VMCS at entry for invalid guest state"],
    "asid": [r"vm C was given VPID \d+, which vm [AB] had, and read its own too"],
    "tpr": [r"wrote 15 to its CR8 and read it back, stopped at each and answered from the shadow, and the host's CR8 is still 0"],
    "cr4": [r"it read back what it wrote, MCE cleared and then PGE set -- the read shadow's -- while the CPU's CR4 "
            r"kept MCE",
            r"PKE and bit 32 each a #GP, and PAE cleared the CPU's own #GP, CR4 as it was",
            r"\b3 of CR4 performed, 2 refused"],
    "dr": [r"vm B read DR0-DR3 as 0 where vm A had left its own, each read back the pattern it wrote",
           r"each one's breakpoint fired at its own address: DR6\.B0, at its ret",
           r"\b18 and 18 movs of debug registers performed, 1 and 1 exceptions given back"],
}


def module(arch):
    ko = pt.module_path(arch, "hv")
    if not os.path.exists(ko):
        sys.exit("no %s -- make modules ARCH=%s first" % (ko, arch))
    return ko


def convert_images(tmp, rootdir):
    """What `hv convert` is given, in the root: qcow2 images qemu-img made of
    a disk made here -- compressed, version 3, clusters of 64 KiB; and
    version 2, of 4 KiB, stored -- the first cut short two thirds into its
    data, and a disk that is raw already. The disk's SHA-256: what `sha256`
    must print of each conversion."""
    if shutil.which("qemu-img") is None:
        sys.exit("hv-test needs qemu-img (QEMU's, beside qemu-system-*) for the hv convert checks")
    rng = random.Random(7)
    disk = os.path.join(tmp, "convert.raw")
    with open(disk, "wb") as f:
        f.truncate(CONVERT_SIZE)
        for _ in range(24):
            at = rng.randrange(CONVERT_SIZE)
            n = min(rng.choice([1, 512, 4096, 65536, 100000, rng.randrange(1, 300000)]), CONVERT_SIZE - at)
            f.seek(at)
            f.write(rng.randbytes(n) if rng.random() < 0.5 else (b"nos hv convert " * (n // 15 + 1))[:n])
        f.seek(CONVERT_SIZE - 512)
        f.write(rng.randbytes(512))
    img = os.path.join(rootdir, "img.qcow2")
    subprocess.run(["qemu-img", "convert", "-f", "raw", "-O", "qcow2", "-c", disk, img], check=True)
    subprocess.run(["qemu-img", "convert", "-f", "raw", "-O", "qcow2", "-o", "compat=0.10,cluster_size=4k", disk,
                    os.path.join(rootdir, "v2.qcow2")], check=True)
    # Past the L2 table at 0x40000, two thirds into the clusters after it:
    # clusters start past the cut, whatever the one it lands in holds.
    cut = os.path.join(rootdir, "cut.qcow2")
    shutil.copy(img, cut)
    data = 0x50000
    os.truncate(cut, data + (os.path.getsize(img) - data) * 2 // 3)
    with open(os.path.join(rootdir, "disk.raw"), "wb") as f:
        f.write(bytes(510) + b"\x55\xaa" + bytes(4096))
    with open(disk, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()


def create_images(tmp, rootdir):
    """What the web server serves `hv create`: the compressed image the
    convert checks have, a raw disk -- a partition table's signature, and
    random bytes -- and a file that is no image. The SHA-256 the two disks
    must have once made: the first's grown to CREATE_SIZE, the second's as
    it is."""
    www = os.path.join(tmp, "www")
    os.makedirs(www)
    shutil.copy(os.path.join(rootdir, "img.qcow2"), www)
    rng = random.Random(11)
    raw = bytearray(rng.randbytes(RAW_SIZE))
    raw[510:512] = b"\x55\xaa"
    with open(os.path.join(www, "raw.img"), "wb") as f:
        f.write(raw)
    with open(os.path.join(www, "notes.txt"), "w") as f:
        f.write("no disk here\n" * 100)
    with open(os.path.join(tmp, "convert.raw"), "rb") as f:
        disk = f.read()
    grown = hashlib.sha256(disk + bytes(CREATE_SIZE - len(disk))).hexdigest()
    return www, grown, hashlib.sha256(raw).hexdigest()


def web_server(root):
    """A web server on the test machine serving `root`, which QEMU's user
    network puts at 10.0.2.2 for nos: the server, and its port."""
    import functools
    import http.server
    import threading

    class Quiet(http.server.SimpleHTTPRequestHandler):
        def log_message(self, *args):
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(Quiet, directory=root))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, server.server_address[1]


def create_checks(out, grown, raw):
    """What `hv create <name> <url>` answered to create_script, in order."""
    pt.check("hv create from a URL: a qcow2 image downloaded, made a 16 MiB disk, its seed given the key and a "
             "password, and a vm.toml written",
             "/vms/c1/vm.toml written -- hv start c1 boots it: hvboot, a 16 MiB disk, cloud-init given 1 key and a "
             "password" in out[0], out[0])
    pt.check("its folder: the disk, the seed and the vm.toml -- the download gone",
             all(f in out[1] for f in ("disk.raw", "seed.iso", "vm.toml")) and "img.qcow2" not in out[1]
             and re.search(r"65536 seed\.iso", out[1]) is not None, out[1])
    pt.check("the disk is the one the image was made of, grown with zeros to size= (its SHA-256)", grown in out[2],
             out[2] + "\nwant " + grown)
    pt.check("the vm.toml boots it by hvboot -- the system image's, unless told -- the disk first and the seed "
             "read-only, on the switch, restarted",
             all(w in out[3] for w in ('kernel = "/lib/hvboot"', "memory = 512",
                                       'disks = ["disk.raw", "seed.iso:ro"]', "net = true", "restart = true")),
             out[3])
    pt.check("a name that has a vm.toml is refused", "is there already -- not written over" in out[4], out[4])
    pt.check("a raw image is taken as it is", "/vms/c2/vm.toml written" in out[5] and "a 2 MiB disk" in out[5], out[5])
    pt.check("byte for byte", raw in out[6], out[6] + "\nwant " + raw)
    pt.check("a URL with nothing at it is refused, saying so", "the server answered 404" in out[7], out[7])
    pt.check("a file that is no image is refused, saying so", "is no image this knows" in out[8], out[8])
    pt.check("a size= under the image's disk is refused", "is less than the image's disk" in out[9], out[9])
    pt.check("and those three left nothing behind",
             "c1" in out[10] and "c2" in out[10] and not any(n in out[10] for n in ("c3", "c4", "c5")), out[10])
    pt.check("hv vms lists the two made", "c1  not running" in out[11] and "c2  not running" in out[11], out[11])


def convert_checks(out, digest):
    """What `hv convert` answered to CONVERT, in order."""
    size_mib = -(-CONVERT_SIZE // (1024 * 1024))
    m = re.search(r"hv: /img\.raw: \d+ MiB written of %d MiB, the rest holes -- (\d+) clusters inflated" % size_mib,
                  out[0])
    pt.check("hv convert: a compressed qcow2 image made a raw disk, its compressed clusters inflated",
             "qcow2 version 3" in out[0] and m is not None and int(m.group(1)) > 0, out[0])
    pt.check("and the disk is the one the image was made of, byte for byte (its SHA-256)", digest in out[1],
             out[1] + "\nwant " + digest)
    pt.check("a conversion over a disk that is there is refused", "is there already -- not written over" in out[2],
             out[2])
    pt.check("a version 2 image of 4 KiB clusters converts", "qcow2 version 2" in out[3] and "MiB written" in out[3],
             out[3])
    pt.check("to the same disk", digest in out[4], out[4] + "\nwant " + digest)
    pt.check("an image cut short is refused, as one", "is the image whole?" in out[5] and "written of" not in out[5],
             out[5])
    pt.check("and leaves no disk behind", "cut.raw" not in out[6] and "img.raw" in out[6], out[6])
    pt.check("a disk that is raw already is refused, saying so", "raw disk" in out[7] and "already" in out[7], out[7])
    pt.check("and hv convert alone says how it is used", "hv convert <image> <disk>" in out[8], out[8])


def rootfs(tmp, arch, rc=None):
    """A root filesystem carrying hv.ko -- and on x86-64 hvboot, the PVH
    image the script boots -- the images `hv convert` is given, and an
    /etc/rc when one is wanted. The image, and the SHA-256 of the disk the
    images are of."""
    rootdir = os.path.join(tmp, "rootdir")
    os.makedirs(os.path.join(rootdir, "etc", "ssh"), exist_ok=True)
    shutil.copy(module(arch), rootdir)
    with open(os.path.join(rootdir, "etc", "ssh", "authorized_keys"), "w") as f:
        f.write("# the key hv create gives cloud-init\n%s\n" % TEST_KEY)
    digest = convert_images(tmp, rootdir)
    if arch == "x86_64":
        if not os.path.exists(HVBOOT):
            sys.exit("no %s -- make first" % HVBOOT)
        shutil.copy(HVBOOT, os.path.join(rootdir, "hvboot"))
        # And where the system image keeps it, which hv create boots by.
        os.makedirs(os.path.join(rootdir, "lib"), exist_ok=True)
        shutil.copy(HVBOOT, os.path.join(rootdir, "lib", "hvboot"))
    if rc is not None:
        with open(os.path.join(rootdir, "etc", "rc"), "w") as f:
            f.write("# scripts/hv-test.py\n" + "\n".join(rc) + "\n")

    image = os.path.join(tmp, "root.img")
    subprocess.run([os.path.join(HERE, "mkrootfs.sh"), image, "64", rootdir, "1024"],
                   cwd=ROOT, check=True, stdout=subprocess.DEVNULL)
    return image, digest


def blocks(log):
    """What each command of the boot script printed. `RunScript` echoes
    "> <command>" before it runs one, so the log splits there."""
    out = []
    body = None
    for line in open(log, errors="replace").read().splitlines():
        if line.startswith("> "):
            body = []
            out.append((line[2:], body))
        elif body is not None:
            body.append(line)
    return [(cmd, "\n".join(lines)) for cmd, lines in out]


def host_flag(flag):
    """Whether the host CPU advertises `flag`: which extension `-cpu host`
    under KVM would hand the guest."""
    try:
        with open("/proc/cpuinfo") as f:
            return re.search(r"^flags\s*:.*\b%s\b" % flag, f.read(), re.M) is not None
    except OSError:
        return False


def host_has_svm():
    """An AMD host, whose `-cpu host` gives the guest AMD-V."""
    return host_flag("svm")


def host_has_vmx():
    """An Intel host, whose `-cpu host` gives the guest Intel VT-x -- nested,
    since nos is itself a guest of KVM. Both backends are tested this way now:
    AMD-V on an AMD host, VT-x on an Intel one, and AMD-V under TCG where
    there is no KVM (TCG has no VT-x)."""
    return host_flag("vmx")


def qemu_version(binary):
    """QEMU's version, as a tuple of ints, or None when it will not say."""
    try:
        out = subprocess.run([binary, "--version"], capture_output=True, text=True).stdout
    except OSError:
        return None
    m = re.search(r"version (\d+)\.(\d+)", out)
    return (int(m.group(1)), int(m.group(2))) if m else None


# The first TCG that puts the accesses of a guest with paging off through
# its nested page table: before it, a real-mode guest's accesses go to the
# host's own memory. The smp guest's second CPU starts in real mode, as
# every CPU a start-up IPI starts does -- so under an older TCG it would run
# out of nos's memory, and corrupt it.
TCG_REAL_MODE = (9, 2)


def x86(args):
    """nos.iso, with the whole script in /etc/rc: there is no remote shell
    on the ISO's command line, and a linear script needs none."""
    kvm = os.path.exists("/dev/kvm") and not args.tcg and (host_has_svm() or host_has_vmx())
    version = qemu_version("qemu-system-x86_64")
    if not kvm and (version is None or version < TCG_REAL_MODE):
        sys.exit("QEMU %s under TCG: the smp guest needs %d.%d or later, which puts a real-mode guest "
                 "through the nested page table -- older, its second CPU would run out of nos's own memory"
                 % (("%d.%d" % version if version else "of unknown version",) + TCG_REAL_MODE))

    tmp = tempfile.mkdtemp(prefix="nos-hv-")
    log = os.path.join(tmp, "serial.log")
    # The images hv create downloads, served before the script is written:
    # its lines say the server's port. The convert checks' images are made
    # into a root of their own first, for it to take one.
    staged = os.path.join(tmp, "staged")
    os.makedirs(staged)
    convert_images(tmp, staged)
    www, grown, raw = create_images(tmp, staged)
    server, port = web_server(www)
    script = SCRIPT + create_script(port) + ["rmmod hv"]
    image, digest = rootfs(tmp, "x86_64", script)

    ext = "the host's AMD-V" if host_has_svm() else "the host's Intel VT-x (nested)"
    print("accelerator: %s" % ("KVM, " + ext if kvm else "TCG, -cpu max (AMD-V)"))
    # -cpu max, not the default: qemu64 reports SVM without nested paging,
    # and a hypervisor that will not shadow page tables has no use for that.
    # Cores of two threads, as the servers' are: under KVM on an AMD host
    # that is what has `hv on` set STIBP on every CPU, which the read-back
    # of IA32_SPEC_CTRL below then holds it to.
    argv = ["qemu-system-x86_64", "-display", "none", "-m", "1G",
            "-smp", "%d,sockets=1,cores=%d,threads=2" % (CPUS, CPUS // 2),
            "-cpu", "host" if kvm else "max",
            "-cdrom", os.path.join(ROOT, "nos.iso"), "-serial", "file:" + log,
            "-drive", "file=%s,format=raw,id=drive0,if=none" % image,
            "-device", "virtio-blk-pci,drive=drive0,disable-legacy=on,disable-modern=off",
            # The network hv create downloads through: the web server above.
            "-device", "virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off",
            "-netdev", "user,id=net0"]
    if kvm:
        argv += ["-enable-kvm"]

    p = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        if not pt.check("boots", pt.wait_log(log, "boot: complete", 400)):
            return
        if not pt.check("the boot script runs to the end",
                        pt.wait_log(log, "> " + script[-1], 200)):
            return
        # The last command's own output lands after the marker above -- which
        # is the first `rmmod hv`'s too: the end of the script is waited for.
        pt.wait_log(log, "rc: /etc/rc done", 600)
        x86_checks(blocks(log), script, digest, grown, raw)
    finally:
        server.shutdown()
        pt.kill(p)
        if args.keep:
            print("log at " + log)
        else:
            shutil.rmtree(tmp, ignore_errors=True)


def guest_report(text, name):
    """What `hv run` said about one guest: from its first line to its verdict."""
    m = re.search(r"^hv: guest %s -- .*?^hv: guest %s .*?$" % (name, name), text, re.M | re.S)
    return m.group(0) if m else ""


# Under VT-x with APIC virtualization (`hv info`): CR8 is the virtual APIC's
# VTPR, never an exit, and an x2APIC guest's interrupts are the CPU's to
# deliver and its EOIs the CPU's to take -- but for the three level-triggered
# ones, which it stops the guest after, for the IO-APIC.
GUESTS_APICV = {
    "tpr": [r"wrote 15 to its CR8 and read it back, the CPU keeping it in the virtual APIC's VTPR, and the "
            r"host's CR8 is still 0"],
    "apicv": [GUESTS["apicv"][0].replace("the host injected all six",
                                         "the CPU delivered all six itself and took their EOIs, stopping the guest "
                                         "after the three level-triggered ones for the IO-APIC"),
              GUESTS["apicv"][1]],
}


# And with posted interrupts too (`hv info`): another CPU's IPIs taken by the
# first in its guest, each notified, the first leaving its guest a handful of
# times rather than once an IPI.
GUESTS_POSTED = {
    "posted": [r"the first CPU took all 1000 of the second's IPIs while it spun, each posted and \d+ notified -- "
               r"taken in its guest, which it left \d+ times",
               r"posted\s+1000 interrupts posted for a CPU to take in its guest"],
}


def check_all_guests(text, cpu, backend, apicv, posted):
    """`hv run all <cpu>`: every guest ran there and did what it was told."""
    guests = dict(GUESTS)
    if backend == "vmx":
        guests.update(GUESTS_VMX)
    if apicv:
        guests.update(GUESTS_APICV)
    if posted:
        guests.update(GUESTS_POSTED)
    m = re.search(r"^hv: (\d+) of (\d+) guests ok$", text, re.M)
    pt.check("every built-in guest does what it was told on cpu %d" % cpu,
             m is not None and m.group(1) == m.group(2) and int(m.group(2)) >= len(guests), text)
    for name, patterns in guests.items():
        report = guest_report(text, name)
        good = ("hv: guest %s ok" % name) in report and all(re.search(p, report) for p in patterns)
        if name in ("smp", "posted"):
            # Its first CPU there, its second on another CPU of its own.
            good = good and re.search(r"ran on\s+cpu %d and (?!%d,)\d+," % (cpu, cpu), report) is not None
        elif name == "avx":
            # Both its CPUs there.
            good = good and re.search(r"ran on\s+cpu %d and cpu %d," % (cpu, cpu), report) is not None
        elif name != "refused":
            good = good and re.search(r"ran on\s+cpu %d," % cpu, report) is not None
        pt.check("  %s, on cpu %d" % (name, cpu), good, report or text)


def x86_checks(ran, script, digest, grown, raw):
    if not pt.check("every line of the script ran", [c for c, _ in ran] == script,
                    "\n".join(c for c, _ in ran)):
        return
    out = [text for _, text in ran]

    pt.check("the module loads", "hv loaded at" in out[0], out[0])

    info = out[1]
    backend = "vmx" if "Intel VT-x" in info else "svm"
    pt.check("the CPU has an extension a guest can run under",
             re.search(r"^hv: .* -- ready$", info, re.M) is not None, info)
    pt.check("with nested paging, which is what makes a guest's memory the CPU's",
             re.search(r"(nested paging|extended page tables)\s+yes", info) is not None, info)
    pt.check("and AVX for a guest, its YMM registers and XCR0 switched with it",
             re.search(r"AVX in a guest\s+yes", info) is not None, info)

    pt.check("it is off for every CPU to begin with",
             "on for cpu none of %s" % ALL_CPUS in out[2], out[2])
    pt.check("and a guest is not run where it is off",
             re.search(r"^hv: guest hypercall not run -- the extension is not on for cpu \d+", out[3], re.M)
             is not None, out[3])

    pt.check("hv on <cpu> turns it on for that CPU", "turned on for cpu 1" in out[4], out[4])
    pt.check("and for no other", "on for cpu 1 of %s" % ALL_CPUS in out[5], out[5])
    pt.check("a guest bound to that CPU runs there",
             "hv: guest hypercall ok" in out[6] and re.search(r"ran on\s+cpu 1,", out[6]) is not None, out[6])
    pt.check("and a guest bound to another is not run",
             "hv: guest hypercall not run -- the extension is not on for cpu 2" in out[7], out[7])

    pt.check("hv off turns it off again", "turned off for cpu 1" in out[8], out[8])
    pt.check("and the CPU says so", "on for cpu none of %s" % ALL_CPUS in out[9], out[9])

    pt.check("hv on turns it on for every CPU",
             "turned on for cpu %s" % ALL_CPUS in out[10], out[10])
    pt.check("and every CPU says so",
             "on for cpu %s of %s" % (ALL_CPUS, ALL_CPUS) in out[11], out[11])

    apicv = re.search(r"APIC virtualization\s+yes", info) is not None
    posted = re.search(r"posted interrupts\s+yes", info) is not None
    print("APIC virtualization: %s, posted interrupts: %s" % ("yes" if apicv else "no", "yes" if posted else "no"))
    check_all_guests(out[12], 0, backend, apicv, posted)
    check_all_guests(out[13], CPUS - 1, backend, apicv, posted)
    pt.check("no guest failed", "FAILED" not in out[12] + out[13], out[12] + out[13])

    # Every guest above ran a VM of its own on CPU 0 and on CPU 3 -- after
    # another guest's there: an IBPB between each two where the CPU has
    # one (`hv::spec`) -- the one after every exit, on an AMD CPU a guest
    # could otherwise steer the host's branches or returns on (BTI,
    # Retbleed, SRSO): Zen 2 under KVM on the AX41 -- and a count of them
    # either way, under both extensions.
    m = re.search(r"guests switched on a CPU (\d+) times(, an IBPB between each two \((\d+) issued\)"
                  r"|; an IBPB after every exit, (\d+) issued, is between each two)?", out[14])
    switches = int(m.group(1)) if m else 0
    pt.check("the guests switched on a CPU at least twice a guest, each switch counted",
             switches >= 2 * len(GUESTS), out[14])
    pt.check("and each with an IBPB where the CPU has one",
             m is not None and (m.group(3) is None and m.group(4) is None and "has no IBPB" in out[14]
                                or m.group(3) is not None and int(m.group(3)) == switches
                                or m.group(4) is not None and int(m.group(4)) >= switches), out[14])
    # Read back from every CPU's register, where `hv on` set one: what it
    # set is still there after the guests.
    pt.check("and every CPU's IA32_SPEC_CTRL still has what hv on set there",
             "IA32_SPEC_CTRL" not in out[14], out[14])

    pt.check("a second insmod is refused", "loaded at" not in out[15], out[15])
    pt.check("a CPU that does not exist is refused", "is not a CPU" in out[16], out[16])
    pt.check("and a word that is not a subcommand", "no such thing" in out[17], out[17])
    pt.check("and a guest that does not exist", "run which guest?" in out[18], out[18])

    pt.check("rmmod with it on for every CPU unloads", "hv unloaded" in out[19], out[19])

    # The one that the rest of them are for: this load asks every CPU what it
    # has, and it has to be nothing -- after every guest above has run.
    pt.check("loading it again finds it off for every CPU", "hv loaded at" in out[20], out[20])
    pt.check("and nothing was left on by the unload before it",
             "on for cpu none of %s" % ALL_CPUS in out[21], out[21])
    pt.check("with no vms= the vms are kept in /vms",
             re.search(r"^hv: vms kept in /vms$", out[21], re.M) is not None, out[21])
    pt.check("it unloads again", "hv unloaded" in out[22], out[22])

    # Its one parameter, vms=: a word it has not got and a path that is no
    # folder's refuse the load, each said; a folder is kept, its slash let go.
    pt.check("a parameter hv has not got refuses the load",
             '"vmz" is not a parameter of hv\'s' in out[23] and "loaded at" not in out[23], out[23])
    pt.check("and so does vms= with no absolute path",
             "vms=data is not an absolute path" in out[24] and "loaded at" not in out[24], out[24])
    pt.check("vms=<folder> loads", "hv loaded at" in out[25], out[25])
    pt.check("and hv says where the vms are kept", "vms kept in /data/vms, as insmod was told" in out[26], out[26])
    pt.check("and hv vms looks there", "no /data/vms" in out[27], out[27])
    pt.check("it unloads", "hv unloaded" in out[28], out[28])
    pt.check("and leaves no module behind", "no modules loaded" in out[29], out[29])

    # What the module traced: not on the serial console, because the shell
    # suppresses trace output there once it starts, so the script asks dmesg.
    logged = out[30]
    pt.check("the kernel log says the extension went off for every CPU on the unload",
             "extension off for cpu mask %s" % ALL_MASK in logged, logged)
    pt.check("and nothing warned", "WARNING" not in logged, logged)

    # A PVH image: hvboot, at its 32-bit entry with paging off, reads the
    # start info hv laid out -- its RAM and the command line, hv's TSC rate
    # after the VM's own words -- finds no disk, says so, and turns the
    # guest off through its ACPI, which is what ends the VM.
    for i, what in ((33, "of one CPU"), (34, "of two CPUs")):
        pvh = out[i]
        pt.check("a PVH image boots, %s: hvboot, at its 32-bit entry, reads the start info hv laid out" % what,
                 re.search(r'hvboot: started by PVH: 6\d MiB of RAM, its command line "hvboot\.verbose', pvh)
                 is not None, pvh)
        pt.check("and, given no disk, says so and turns the guest off by its ACPI",
                 "hvboot: cannot boot: no virtio disk" in pvh and "the guest powered itself off" in pvh, pvh)
    pt.check("and hv unloads after it", "hv unloaded" in out[35], out[35])

    pt.check("hv loads for hv convert", "hv loaded at" in out[36], out[36])
    convert_checks(out[37:37 + len(CONVERT)], digest)
    at = 37 + len(CONVERT)
    created = len(script) - 1 - at
    create_checks(out[at:at + created], grown, raw)
    pt.check("and unloads after it", "hv unloaded" in out[at + created], out[at + created])


def arm64(args):
    """The arm64 boot has a remote shell on its command line, so the checks
    run one command at a time rather than from a script."""
    tmp = tempfile.mkdtemp(prefix="nos-hv-")
    log = os.path.join(tmp, "serial.log")
    image, digest = rootfs(tmp, "aarch64")

    hvf = platform.system() == "Darwin" and platform.machine() == "arm64" and not args.tcg
    accel = ["-accel", "hvf", "-cpu", "host"] if hvf else ["-accel", "tcg", "-cpu", "cortex-a72"]
    argv = ["qemu-system-aarch64", "-M", "virt,gic-version=3", "-smp", str(CPUS),
            "-m", "1024"] + accel + [
        "-kernel", os.path.join(ROOT, "nos-arm64.img"),
        "-append", "root=auto udpshell=%d" % SHELL_PORT,
        "-global", "virtio-mmio.force-legacy=false",
        "-drive", "file=%s,format=raw,id=hd0,if=none" % image,
        "-device", "virtio-blk-device,drive=hd0",
        "-device", "virtio-net-device,netdev=net0",
        "-netdev", "user,id=net0,hostfwd=udp::%d-:%d" % (SHELL_PORT, SHELL_PORT),
        "-serial", "file:" + log, "-display", "none"]

    p = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        if not pt.check("boots", pt.wait_log(log, "boot: complete", 120 if hvf else 300)):
            return
        sh = pt.Shell()
        refused = sh.run("insmod /hv.ko vms=/data/vms")
        pt.check("given a parameter, the module refuses the load -- it takes none here",
                 "takes none" in refused and "loaded at" not in refused, refused)
        pt.check("the module loads", "hv loaded at" in sh.run("insmod /hv.ko"))

        info = sh.run("hv info")
        pt.check("and says a guest cannot run here", "not implemented" in info, info)
        pt.check("naming the exception level it is at", re.search(r"running at\s+EL1", info) is not None, info)
        pt.check("and what stage-2 would translate into",
                 re.search(r"stage-2 output\s+\d+ bits", info) is not None, info)

        status = sh.run("hv")
        pt.check("its status says the same", "no guest can run here" in status, status)
        on = sh.run("hv on")
        pt.check("hv on is refused rather than attempted", "cannot turn it on" in on, on)
        guests = sh.run("hv run all")
        pt.check("and so is hv run", "no guest can run here" in guests, guests)
        # What needs no guest runs here as on x86-64: a disk made of an image.
        convert_checks([sh.run(cmd) for cmd in CONVERT], digest)

        pt.check("it unloads", "hv unloaded" in sh.run("rmmod hv"))
        pt.check("and leaves no module behind", "no modules loaded" in sh.run("lsmod"))
    finally:
        pt.kill(p)
        if args.keep:
            print("log at " + log)
        else:
            shutil.rmtree(tmp, ignore_errors=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--arch", choices=("x86_64", "aarch64"), default="x86_64")
    ap.add_argument("--tcg", action="store_true", help="do not use KVM or HVF")
    ap.add_argument("--keep", action="store_true", help="keep the serial log")
    args = ap.parse_args()

    if args.arch == "x86_64":
        x86(args)
    else:
        arm64(args)

    print()
    if pt.failures:
        print("FAILED: " + ", ".join(pt.failures))
        return 1
    print("hv-test: OK (%s)" % args.arch)
    return 0


if __name__ == "__main__":
    sys.exit(main())
