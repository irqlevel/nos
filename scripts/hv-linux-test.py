#!/usr/bin/env python3
"""hv Linux test: load a real Linux bzImage into a guest under the nos
hypervisor and watch its serial console come out -- once for a set time
(`hv boot`), and then as a VM that runs until it is stopped (`hv start`)
and is typed at while it runs.

This is the third and fourth of the four demos in plans/03-hypervisor.md --
a bzImage printing its early console, and booting to a shell that runs a
command -- and it is a *manual* gate, like the hardware NIC ones: it needs a
bzImage, which is megabytes and which CI cannot build in the time it has, so
it is not in ci.yml and is pointed at a kernel by hand.

Build a small 64-bit guest kernel (a tinyconfig with 8250 serial and an early
console is enough; PCI, ACPI and SMP off), then:

    scripts/hv-linux-test.py --bzimage /path/to/bzImage
    scripts/hv-linux-test.py --bzimage bzImage --initrd initramfs.cpio.gz \\
        --cmdline "earlyprintk=serial,ttyS0,115200 console=ttyS0 nolapic no_timer_check"

What it checks: nos boots, the hv module loads and turns the extension on,
`hv boot` streams the guest's console to the kernel log (hvguest| ...), and
the guest gets far enough into early boot to print the kernel banner and set
up its memory -- under AMD-V, which on a machine with no hardware SVM is
QEMU's TCG (slow: the whole run is a minute or two).

With an initrd, then the long-lived VMs: two started side by side, one
stopped while it boots; a line typed at the other before its shell is up,
answered once it is (`hv exec`), and one typed at its prompt; `hv send` and
`hv wait`; `hv console`; `hv off` refusing to pull the extension from under
a running guest; and `rmmod` stopping that guest itself, with nothing left
on for the next load to find.

Exit code 0 = every required marker appeared.
"""

import argparse
import importlib.util
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

spec = importlib.util.spec_from_file_location("hvt", os.path.join(HERE, "hv-test.py"))
hvt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hvt)
pt = hvt.pt

# The lines the guest has to reach: the banner, and enough of early setup to
# show it is a real kernel running and not just the decompressor stub. Each
# is matched inside the guest's console as `hv boot` reports it, between its
# ttyS0 markers, so nothing of nos's own console can satisfy one.
REQUIRED = [
    r"Linux version",
    r"Command line:",
    r"NX \(Execute Disable\) protection: active",
    r"console \[ttyS0\] enabled",
]
# A later line that shows the guest got well past the banner into device
# setup; not required, but reported when it appears.
NICE = r"Kernel command line:"

# With an initramfs, the fourth demo: the guest runs its init and reaches an
# interactive shell over the emulated UART. These are checked only when an
# initrd is given (the init here is a busybox that prints and execs a shell).
SHELL_MARKERS = [
    r"Run /init as init process",
    r"BusyBox",
]

DEFAULT_CMDLINE = "earlyprintk=serial,ttyS0,115200 console=ttyS0 nolapic no_timer_check"

# The last line of /etc/rc, and how the console shows it has run: its echo
# and the line it prints. Once the shell has the console, the kernel's own
# log -- "rc: /etc/rc done", and every trace line -- goes to dmesg and not
# to the console, so the end is marked with a command's output instead.
RC_LAST = "version"
RC_DONE = re.compile(r"(?m)^> version\n.+\n")
# The module's own lines from the kernel log, printed to the console: what
# the vCPU tasks and the unload say, which no command's output does.
RC_LOG = "dmesg hv:"


def vm_commands(args):
    """The long-lived VM phase, as /etc/rc lines, and what each must print.

    Each check is (command, occurrence, what, pattern, must): the output of
    the occurrence-th run of that line must (or must not) match."""
    start0 = "hv start /bzImage mem=%d initrd=/initrd cmdline=%s" % (args.mem, args.cmdline)
    start1 = "hv start /bzImage mem=64 cmdline=%s" % args.cmdline
    # Typed while vm 0 still boots: held until its shell asks for input,
    # and answered at the prompt printed after the line went in.
    exec_early = "hv exec 0 secs=%d id" % args.vm_secs
    exec_prompt = "hv exec 0 uname -r"
    send = r"hv send 0 echo nos$((6*7))nos\n"
    wait = "hv wait 0 secs=120 nos42nos"
    # A guest that reboots itself: no init, so it panics, and panic=1 makes
    # the panic a reboot a second later. `hv wait` for text it never prints
    # returns when the guest stops, saying why.
    start2 = "hv start /bzImage mem=64 cmdline=%s panic=1" % args.cmdline
    wait_reboot = "hv wait 2 secs=300 Rebooting in"
    wait_stop = "hv wait 2 secs=300 nos-never-printed"
    # The same stopped guest booted again by hand; then one with `restart`,
    # booted again by itself each time it resets -- until the sixth reset in
    # a minute says it is a loop; then the running guest reset by hand, and
    # typed at again once it is back.
    wait_stop2 = "hv wait 2 secs=300 nos-never-printed-2"
    start3 = "hv start /bzImage mem=64 restart cmdline=%s panic=1" % args.cmdline
    wait_loop = "hv wait 3 secs=500 nos-never-printed-3"
    exec_again = "hv exec 0 secs=%d id" % args.vm_secs
    lines = ["hv help", start0, start1, "hv list", "hv stop 1", exec_early, exec_prompt,
             send, wait, "hv console 0 bytes=400", "hv list",
             start2, wait_reboot, wait_stop, "hv list",
             "hv restart 2", wait_stop2, "hv list", "hv stop 2",
             start3, wait_loop, "hv list", "hv stop 3",
             "hv restart 0", exec_again, "hv list",
             "hv off", "rmmod hv", "insmod /hv.ko", "hv", "rmmod hv", RC_LOG]
    checks = [
        ("hv help", 0, "hv help lists the vm commands", r"hv exec <id>", True),
        (start0, 0, "vm 0 starts", r"hv: vm 0 started on cpu \d+", True),
        (start1, 0, "vm 1 starts beside it", r"hv: vm 1 started on cpu \d+", True),
        ("hv list", 0, "hv list shows vm 0 running", r"vm 0  running", True),
        ("hv list", 0, "hv list shows vm 1 running", r"vm 1  running", True),
        ("hv stop 1", 0, "vm 1 stops on request, mid-boot", r"hv: vm 1 stopped -- on request", True),
        (exec_early, 0, "a line typed during boot is answered at the prompt after it",
         r"# id\nuid=0 gid=0\n", True),
        (exec_early, 0, "... and nothing was left waiting", r"hv: vm 0: ", False),
        (exec_prompt, 0, "a line typed at the prompt comes back whole", r"(?m)^uname -r\n\d+\.\d+", True),
        (send, 0, "hv send queues", r"hv: vm 0: \d+ bytes queued", True),
        (wait, 0, "hv wait sees what the sent line printed", r'hv: vm 0 printed "nos42nos"', True),
        ("hv console 0 bytes=400", 0, "hv console has it too", r"nos42nos", True),
        ("hv list", 1, "vm 0 still running", r"vm 0  running", True),
        ("hv list", 1, "vm 1 gone from the list", r"vm 1 ", False),
        (start2, 0, "vm 2 starts, to panic and reboot", r"hv: vm 2 started on cpu \d+", True),
        (wait_reboot, 0, "vm 2 panics and goes to reboot", r'hv: vm 2 printed "Rebooting in"', True),
        (wait_stop, 0, "its reset stops it, and says so",
         r"hv: vm 2 stopped without printing .* -- the guest asked for a reset, 0xfe to port 0x64", True),
        ("hv list", 2, "hv list keeps it, stopped, with its reason",
         r"vm 2  stopped .* -- the guest asked for a reset", True),
        ("hv stop 2", 0, "hv stop takes it off the list", r"hv: vm 2 stopped -- the guest asked for a reset", True),
        ("hv restart 2", 0, "hv restart boots a stopped guest again", r"hv: vm 2 restarted", True),
        (wait_stop2, 0, "... which resets again, and stops again",
         r"hv: vm 2 stopped without printing .* -- the guest asked for a reset", True),
        ("hv list", 3, "hv list counts the restart", r"vm 2  stopped .* restarts 1 ", True),
        (start3, 0, "a guest with restart starts", r"hv: vm 3 started on cpu \d+ .*restarted when it resets", True),
        (wait_loop, 0, "it is booted again at each reset, until the loop is called one",
         r"hv: vm 3 stopped without printing .* -- the guest asked for a reset.* -- reset 6 times in 60 s, left stopped", True),
        ("hv list", 4, "five restarts before it was left stopped", r"vm 3  stopped .* restarts 5 ", True),
        ("hv restart 0", 0, "hv restart resets the running guest", r"hv: vm 0 restarted", True),
        (exec_again, 0, "and its new boot answers a line typed during it", r"# id\nuid=0 gid=0\n", True),
        ("hv list", 5, "running again, one restart counted", r"vm 0  running .* restarts 1 ", True),
        ("hv off", 0, "hv off will not pull the extension from under vm 0",
         r"hv: vm 0 is running on cpu \d+ -- hv stop it first", True),
        ("hv", 0, "the next load finds it off everywhere", r"on for cpu none of", True),
        (RC_LOG, 0, "vm 1's vCPU said how it ended", r"hv: vm 1 stopped after \d+ ms -- on request", True),
        (RC_LOG, 0, "rmmod stopped vm 0 itself, and only then turned the extension off",
         r"hv: vm 0 stopped for the unload -- on request[\s\S]*hv: unloaded, extension off for cpu mask", True),
        (RC_LOG, 0, "no unload left the extension on", r"hv: WARNING", False),
    ]
    return lines, checks


def sections(txt):
    """What each /etc/rc line printed, in order: (line, output) pairs, the
    output being everything on the console up to the next line rc echoes --
    its own output as printed, then the same again in the log, and the
    kernel's own lines in between."""
    parts = re.split(r"(?m)^> (.*)$", txt)
    return [(parts[i].strip(), parts[i + 1]) for i in range(1, len(parts) - 1, 2)]


def output_of(secs, line, occurrence):
    runs = [out for (cmd, out) in secs if cmd == line]
    return runs[occurrence] if occurrence < len(runs) else None


def rootfs(tmp, bzimage, initrd, rc):
    rootdir = os.path.join(tmp, "rootdir")
    os.makedirs(os.path.join(rootdir, "etc"), exist_ok=True)
    shutil.copy(hvt.module("x86_64"), rootdir)
    shutil.copy(bzimage, os.path.join(rootdir, "bzImage"))
    if initrd:
        shutil.copy(initrd, os.path.join(rootdir, "initrd"))
    with open(os.path.join(rootdir, "etc", "rc"), "w") as f:
        f.write("# scripts/hv-linux-test.py\n" + "\n".join(rc) + "\n")

    image = os.path.join(tmp, "root.img")
    # A roomy filesystem: the kernel and initrd have to fit on it.
    subprocess.run([os.path.join(HERE, "mkrootfs.sh"), image, "128", rootdir],
                   cwd=ROOT, check=True, stdout=subprocess.DEVNULL)
    return image


def check_boot(args, txt):
    """The one-shot `hv boot`: its report block, and the console in it."""
    if not pt.check("the guest ran and its console came back",
                    "--- end ttyS0 ---" in txt or "the guest printed nothing" in txt,
                    txt[-1500:]):
        return
    block = txt[txt.find("--- ttyS0 ---"):txt.find("--- end ttyS0 ---")] if "--- ttyS0 ---" in txt else ""
    for m in REQUIRED:
        pt.check("the guest printed: %s" % m, re.search(m, block) is not None, block[-1500:])
    if re.search(NICE, block):
        print("note: the guest reached %r" % NICE)
    if args.initrd:
        for m in SHELL_MARKERS:
            pt.check("the guest booted to a shell: %s" % m, re.search(m, block) is not None,
                     block[-1500:])
    for m in args.expect:
        pt.check("the console shows: %s" % m, re.search(re.escape(m), block) is not None,
                 block[-1500:])


def free_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def attach(args):
    """`hv attach` over a real SSH session: sshd and a guest started from
    /etc/rc, then a line typed through `ssh -tt ... hv attach 0` and its
    answer read back, ^] to detach -- and from /etc/rc, where nobody can
    type, a refusal."""
    tmp = tempfile.mkdtemp(prefix="nos-hvattach-")
    log = os.path.join(tmp, "serial.log")
    key = os.path.join(tmp, "id_test")
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "hv-linux-test", "-f", key], check=True)
    port = free_port()

    rootdir = os.path.join(tmp, "rootdir")
    os.makedirs(os.path.join(rootdir, "etc", "ssh"))
    shutil.copy(hvt.module("x86_64"), rootdir)
    shutil.copy(os.path.join(ROOT, "out", "x86_64", "modules", "sshd.ko"), rootdir)
    shutil.copy(args.bzimage, os.path.join(rootdir, "bzImage"))
    shutil.copy(args.initrd, os.path.join(rootdir, "initrd"))
    with open(os.path.join(rootdir, "etc", "ssh", "authorized_keys"), "w") as f:
        f.write(open(key + ".pub").read())
    rc = ["insmod /sshd.ko", "sshd start", "insmod /hv.ko", "hv on",
          "hv start /bzImage mem=%d initrd=/initrd cmdline=%s" % (args.mem, args.cmdline),
          "hv exec 0 secs=%d id" % args.vm_secs, "hv attach 0", RC_LAST]
    with open(os.path.join(rootdir, "etc", "rc"), "w") as f:
        f.write("# scripts/hv-linux-test.py --attach\n" + "\n".join(rc) + "\n")
    image = os.path.join(tmp, "root.img")
    subprocess.run([os.path.join(HERE, "mkrootfs.sh"), image, "128", rootdir],
                   cwd=ROOT, check=True, stdout=subprocess.DEVNULL)

    argv = ["qemu-system-x86_64", "-display", "none", "-m", "2G", "-smp", "4", "-cpu", "max",
            "-cdrom", os.path.join(ROOT, "nos.iso"), "-serial", "file:" + log,
            "-drive", "file=%s,format=raw,id=drive0,if=none" % image,
            "-device", "virtio-blk-pci,drive=drive0,disable-legacy=on,disable-modern=off",
            "-device", "virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off",
            "-netdev", "user,id=net0,hostfwd=tcp:127.0.0.1:%d-:22" % port]
    ssh = ["ssh", "-p", str(port), "-i", key, "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes",
           "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
           "-o", "ConnectTimeout=60", "-o", "LogLevel=ERROR"]
    p = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        done = False
        t0 = time.time()
        txt = ""
        while time.time() - t0 < args.deadline and p.poll() is None:
            txt = open(log, errors="replace").read() if os.path.exists(log) else ""
            if "PANIC:" in txt:
                pt.check("nos did not panic", False, txt[-3000:])
                return
            if RC_DONE.search(txt):
                done = True
                break
            time.sleep(3)
        if not pt.check("sshd and a guest up, from /etc/rc", done,
                        txt[-2000:] or "qemu exited %s" % p.poll()):
            return
        secs = sections(open(log, errors="replace").read())
        pt.check("the guest's shell answered", re.search(r"uid=0 gid=0", output_of(secs, rc[5], 0) or "") is not None)
        pt.check("hv attach refuses where nobody can type",
                 "nobody can type here" in (output_of(secs, "hv attach 0", 0) or ""))

        # A person at a terminal: a line typed, its answer awaited, ^] typed.
        a = subprocess.Popen(ssh + ["-tt", "root@127.0.0.1", "hv attach 0"], stdin=subprocess.PIPE,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        time.sleep(args.attach_wait)
        a.stdin.write(b"echo nos$((6*7))attach\r")
        a.stdin.flush()
        time.sleep(args.attach_wait)
        a.stdin.write(b"\x1d")
        a.stdin.flush()
        try:
            out, err = a.communicate(timeout=120)
        except subprocess.TimeoutExpired:
            a.kill()
            out, err = a.communicate()
        text = out.decode(errors="replace")
        pt.check("hv attach says it is attached", "attached to vm 0" in text, repr(text[-600:]))
        pt.check("what was typed ran in the guest, its answer came back", "nos42attach" in text, repr(text[-600:]))
        pt.check("^] detaches", "hv: detached -- vm 0" in text, repr(text[-600:]))
        pt.check("no terminal query reached the terminal", b"\x1b[6n" not in out, repr(text[-600:]))
        pt.check("the session ended cleanly", a.returncode == 0, "exit %s, %r" % (a.returncode, err[-300:]))

        c = subprocess.run(ssh + ["root@127.0.0.1", "hv list"], capture_output=True, timeout=120)
        pt.check("the guest runs on after the detach", b"vm 0  running" in c.stdout, repr(c.stdout[-300:]))
    finally:
        pt.kill(p)
        if args.keep or pt.failures:
            print("log at " + log)
        else:
            shutil.rmtree(tmp, ignore_errors=True)


def run(args):
    tmp = tempfile.mkdtemp(prefix="nos-hvlinux-")
    log = os.path.join(tmp, "serial.log")

    boot_cmd = "hv boot /bzImage mem=%d secs=%d" % (args.mem, args.secs)
    if args.initrd:
        boot_cmd += " initrd=/initrd"
    if args.input:
        # A single token, no spaces: \n stands for a newline. Kept before
        # cmdline=, which takes the rest of the line.
        boot_cmd += " input=" + args.input
    boot_cmd += " cmdline=" + args.cmdline
    rc = ["insmod /hv.ko", "hv on"]
    if not args.skip_boot:
        rc.append(boot_cmd)
    vm_lines, vm_checks = vm_commands(args) if args.initrd and not args.skip_vms else ([], [])
    rc += vm_lines + [RC_LAST]
    for line in rc:
        if len(line) > 255:
            sys.exit("an /etc/rc line is longer than rc takes (255): " + line)
    image = rootfs(tmp, args.bzimage, args.initrd, rc)

    kvm = os.path.exists("/dev/kvm") and not args.tcg and hvt.host_has_svm()
    print("accelerator: %s" % ("KVM, the host's AMD-V" if kvm else "TCG, -cpu max"))
    argv = ["qemu-system-x86_64", "-display", "none", "-m", "2G", "-smp", "4",
            "-cpu", "host" if kvm else "max",
            "-cdrom", os.path.join(ROOT, "nos.iso"), "-serial", "file:" + log,
            "-drive", "file=%s,format=raw,id=drive0,if=none" % image,
            "-device", "virtio-blk-pci,drive=drive0,disable-legacy=on,disable-modern=off"]
    if kvm:
        argv += ["-enable-kvm"]

    p = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        if not pt.check("nos boots and runs /etc/rc",
                        pt.wait_log(log, "> hv on", args.deadline)):
            return
        # Every line of /etc/rc, to its end: the one-shot boot runs its whole
        # budget, and the VM phase waits on the guest as it goes.
        done = False
        t0 = time.time()
        while time.time() - t0 < args.deadline:
            txt = open(log, errors="replace").read()
            if "PANIC:" in txt:
                pt.check("nos did not panic", False, txt[-3000:])
                return
            if RC_DONE.search(txt):
                done = True
                break
            time.sleep(3)
        txt = open(log, errors="replace").read()
        if not pt.check("/etc/rc ran to its end", done, txt[-2000:]):
            return

        if not args.skip_boot:
            check_boot(args, txt)
        secs = sections(txt)
        for (line, occurrence, what, pattern, must) in vm_checks:
            out = output_of(secs, line, occurrence)
            if out is None:
                pt.check(what, False, "no output for %r" % line)
                continue
            found = re.search(pattern, out) is not None
            pt.check(what, found == must, out[-1500:])
    finally:
        pt.kill(p)
        if args.verbose and os.path.exists(log):
            print(open(log, errors="replace").read())
        if args.keep or pt.failures:
            print("log at " + log)
        else:
            shutil.rmtree(tmp, ignore_errors=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bzimage", required=True, help="a 64-bit Linux bzImage")
    ap.add_argument("--initrd", help="an initramfs image, optional")
    ap.add_argument("--cmdline", default=DEFAULT_CMDLINE, help="the guest kernel command line")
    ap.add_argument("--mem", type=int, default=256, help="guest RAM in MiB")
    ap.add_argument("--secs", type=int, default=120, help="guest run budget in seconds")
    ap.add_argument("--input", help="a single no-space token typed at the guest console once up; \\n = newline")
    ap.add_argument("--expect", action="append", default=[], help="extra text the console must contain (repeatable)")
    ap.add_argument("--deadline", type=int, default=900, help="seconds to wait for /etc/rc to finish")
    ap.add_argument("--vm-secs", type=int, default=400,
                    help="how long the first hv exec waits for the started guest's shell (at most 600)")
    ap.add_argument("--skip-boot", action="store_true", help="leave out the one-shot hv boot")
    ap.add_argument("--skip-vms", action="store_true", help="leave out the hv start phase")
    ap.add_argument("--attach", action="store_true",
                    help="instead: hv attach over a real ssh -tt session (needs ssh, ssh-keygen and --initrd)")
    ap.add_argument("--attach-wait", type=float, default=5.0,
                    help="seconds between what is typed through hv attach, for the guest to answer")
    ap.add_argument("--tcg", action="store_true", help="do not use KVM even if the host has AMD-V")
    ap.add_argument("--keep", action="store_true", help="keep the serial log (it is kept on a failure anyway)")
    ap.add_argument("--verbose", action="store_true", help="print the whole serial log at the end")
    args = ap.parse_args()

    if not os.path.exists(args.bzimage):
        sys.exit("no such bzImage: " + args.bzimage)
    if not os.path.exists(os.path.join(ROOT, "nos.iso")):
        sys.exit("build nos.iso first (make)")

    if args.attach:
        if not args.initrd:
            sys.exit("--attach needs --initrd: the guest's shell is what is typed at")
        attach(args)
    else:
        run(args)

    print()
    if pt.failures:
        print("FAILED: " + ", ".join(pt.failures))
        return 1
    print("hv-linux-test: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
