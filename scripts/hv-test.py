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
    guest, a VMCB that breaks a rule refused with the rule named, and a
    `cli; jmp $` the host's interrupts get through and the host stops
  - a second `insmod`, a CPU that does not exist, a word that is not a
    subcommand and a guest that does not exist are each refused
  - `rmmod` with the extension on for every CPU turns it off for every CPU
  - loading it again finds nothing left on -- the check the rest is for
  - nothing warned, and nothing panicked

and on arm64, where a hypervisor needs EL2 and this kernel dropped to EL1 in
its first hundred instructions:

  - the module loads, says so, and refuses to run anything
  - `hv info` reports the exception level and what stage-2 would offer
  - `hv on` and `hv run` are refused rather than attempted
  - it unloads

    scripts/hv-test.py [--arch x86_64|aarch64] [--tcg] [--keep]

Exit code 0 = every check passed.
"""

import argparse
import importlib.util
import os
import platform
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
    "insmod /hv.ko",        # 14
    "hv on 99",             # 15
    "hv nonsense",          # 16
    "hv run nonsense",      # 17
    "rmmod hv",             # 18
    "insmod /hv.ko",        # 19
    "hv",                   # 20
    "rmmod hv",             # 21
    "lsmod",                # 22
    "dmesg 400 hv:",        # 23
]

# What `hv run all` has to have said about each guest, beyond its verdict.
GUESTS = {
    "exits": [r'said\s+"nos: ports and cpuid"', r"1 port in, 20 port out, 1 cpuid"],
    "hypercall": [r'said\s+"nos: long mode"', r"15 registers went out at the hypercall and 15 answers came back"],
    "fault": [r"nested page fault at gpa 0x1ff000, error 0x1[0-9a-f]{8}, rip 0x8005"],
    "triple": [r"shutdown \(triple fault\)"],
    "refused": [r"not entered -- the VMCB breaks a rule: CR0.NW is set without CR0.CD"],
    "spin": [r"stopped\s+by the host", r"interrupts got through [1-9]\d* times"],
}


def module(arch):
    ko = os.path.join(ROOT, "out", arch, "modules", "hv.ko")
    if not os.path.exists(ko):
        sys.exit("no %s -- make modules ARCH=%s first" % (ko, arch))
    return ko


def rootfs(tmp, arch, rc=None):
    """A root filesystem carrying hv.ko, and an /etc/rc when one is wanted."""
    rootdir = os.path.join(tmp, "rootdir")
    os.makedirs(os.path.join(rootdir, "etc"), exist_ok=True)
    shutil.copy(module(arch), rootdir)
    if rc is not None:
        with open(os.path.join(rootdir, "etc", "rc"), "w") as f:
            f.write("# scripts/hv-test.py\n" + "\n".join(rc) + "\n")

    image = os.path.join(tmp, "root.img")
    subprocess.run([os.path.join(HERE, "mkrootfs.sh"), image, "64", rootdir, "1024"],
                   cwd=ROOT, check=True, stdout=subprocess.DEVNULL)
    return image


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


def host_has_svm():
    """Whether KVM would hand the guest AMD-V. `-cpu host` gives it the host
    CPU's own extension, and the guests here run under AMD-V: on an Intel
    host that is VT-x, whose guests are not written yet -- there, TCG's
    AMD-V is the one that runs them."""
    try:
        with open("/proc/cpuinfo") as f:
            return re.search(r"^flags\s*:.*\bsvm\b", f.read(), re.M) is not None
    except OSError:
        return False


def x86(args):
    """nos.iso, with the whole script in /etc/rc: there is no remote shell
    on the ISO's command line, and a linear script needs none."""
    tmp = tempfile.mkdtemp(prefix="nos-hv-")
    log = os.path.join(tmp, "serial.log")
    image = rootfs(tmp, "x86_64", SCRIPT)

    kvm = os.path.exists("/dev/kvm") and not args.tcg and host_has_svm()
    print("accelerator: %s" % ("KVM, the host's AMD-V" if kvm else "TCG, -cpu max"))
    # -cpu max, not the default: qemu64 reports SVM without nested paging,
    # and a hypervisor that will not shadow page tables has no use for that.
    argv = ["qemu-system-x86_64", "-display", "none", "-m", "1G", "-smp", str(CPUS),
            "-cpu", "host" if kvm else "max",
            "-cdrom", os.path.join(ROOT, "nos.iso"), "-serial", "file:" + log,
            "-drive", "file=%s,format=raw,id=drive0,if=none" % image,
            "-device", "virtio-blk-pci,drive=drive0,disable-legacy=on,disable-modern=off"]
    if kvm:
        argv += ["-enable-kvm"]

    p = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        if not pt.check("boots", pt.wait_log(log, "boot: complete", 400)):
            return
        if not pt.check("the boot script runs to the end",
                        pt.wait_log(log, "> " + SCRIPT[-1], 200)):
            return
        # The last command's own output lands after the marker above.
        pt.wait_log(log, "rc: /etc/rc done", 60)
        x86_checks(blocks(log))
    finally:
        pt.kill(p)
        if args.keep:
            print("log at " + log)
        else:
            shutil.rmtree(tmp, ignore_errors=True)


def guest_report(text, name):
    """What `hv run` said about one guest: from its first line to its verdict."""
    m = re.search(r"^hv: guest %s -- .*?^hv: guest %s .*?$" % (name, name), text, re.M | re.S)
    return m.group(0) if m else ""


def check_all_guests(text, cpu):
    """`hv run all <cpu>`: every guest ran there and did what it was told."""
    m = re.search(r"^hv: (\d+) of (\d+) guests ok$", text, re.M)
    pt.check("every built-in guest does what it was told on cpu %d" % cpu,
             m is not None and m.group(1) == m.group(2) and int(m.group(2)) >= len(GUESTS), text)
    for name, patterns in GUESTS.items():
        report = guest_report(text, name)
        good = ("hv: guest %s ok" % name) in report and all(re.search(p, report) for p in patterns)
        if name != "refused":
            good = good and re.search(r"ran on\s+cpu %d," % cpu, report) is not None
        pt.check("  %s, on cpu %d" % (name, cpu), good, report or text)


def x86_checks(ran):
    if not pt.check("every line of the script ran", [c for c, _ in ran] == SCRIPT,
                    "\n".join(c for c, _ in ran)):
        return
    out = [text for _, text in ran]

    pt.check("the module loads", "hv loaded at" in out[0], out[0])

    info = out[1]
    pt.check("the CPU has an extension a guest can run under",
             re.search(r"^hv: .* -- ready$", info, re.M) is not None, info)
    pt.check("with nested paging, which is what makes a guest's memory the CPU's",
             re.search(r"(nested paging|extended page tables)\s+yes", info) is not None, info)

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

    check_all_guests(out[12], 0)
    check_all_guests(out[13], CPUS - 1)
    pt.check("no guest failed", "FAILED" not in out[12] + out[13], out[12] + out[13])

    pt.check("a second insmod is refused", "loaded at" not in out[14], out[14])
    pt.check("a CPU that does not exist is refused", "is not a CPU" in out[15], out[15])
    pt.check("and a word that is not a subcommand", "no such thing" in out[16], out[16])
    pt.check("and a guest that does not exist", "run which guest?" in out[17], out[17])

    pt.check("rmmod with it on for every CPU unloads", "hv unloaded" in out[18], out[18])

    # The one that the rest of them are for: this load asks every CPU what it
    # has, and it has to be nothing -- after every guest above has run.
    pt.check("loading it again finds it off for every CPU", "hv loaded at" in out[19], out[19])
    pt.check("and nothing was left on by the unload before it",
             "on for cpu none of %s" % ALL_CPUS in out[20], out[20])
    pt.check("it unloads again", "hv unloaded" in out[21], out[21])
    pt.check("and leaves no module behind", "no modules loaded" in out[22], out[22])

    # What the module traced: not on the serial console, because the shell
    # suppresses trace output there once it starts, so the script asks dmesg.
    logged = out[23]
    pt.check("the kernel log says the extension went off for every CPU on the unload",
             "extension off for cpu mask %s" % ALL_MASK in logged, logged)
    pt.check("and nothing warned", "WARNING" not in logged, logged)


def arm64(args):
    """The arm64 boot has a remote shell on its command line, so the checks
    run one command at a time rather than from a script."""
    tmp = tempfile.mkdtemp(prefix="nos-hv-")
    log = os.path.join(tmp, "serial.log")
    image = rootfs(tmp, "aarch64")

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
