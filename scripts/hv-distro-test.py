#!/usr/bin/env python3
"""hv distro test: a Linux distribution booted under the nos hypervisor as
it ships -- Alpine's `virt` ISO, its own kernel and initramfs and its
packages, unmodified -- logged into, rebooted, networked, and reached from
outside over ssh by its own sshd.

hv-linux-test boots a kernel built for the purpose, with a BusyBox
initramfs. This is the other kind of guest: a kernel configured for every
machine rather than this one (SMP, ACPI, KASLR, high-resolution timers,
virtio as modules), an initramfs that finds its boot medium on a disk by
itself, OpenRC, a getty, apk. Alpine's `virt` flavour is the smallest
distribution that is all of that, and boots from its ISO as a read-only
virtio disk:

  - the kernel and the initramfs are taken out of the ISO, and the ISO
    itself is the guest's `vda` (`disk=...:ro`): the initramfs loads
    virtio_blk, mounts the ISO, installs the base system from its packages
    into a tmpfs, and OpenRC brings it up to a login prompt on ttyS0
  - root logs in (`hv send`, which types at a login prompt -- it asks for no
    cursor, as a shell's line editor does), and the guest says what it is
  - its clock is the host's (the emulated RTC), to the minute
  - the ISO is read-only to it: the driver says so and a write fails
  - `reboot` resets it through the keyboard controller, as a PC does; with
    `restart` the VM boots again, and root logs in again
  - on the guests' switch (`net`), its initramfs configures eth0 from the
    `ip=` the VM is given; it pings nos at 10.0.100.1 and nos pings it
  - `apk add openssh-server` from the ISO, a key authorised, sshd started,
    `hv forward` from nos's port 2222 to its port 22 -- and the test logs
    into the guest from outside, through QEMU's forward, nos's relay and the
    switch

Manual, like hv-linux-test: the ISO is a download CI does not make, and the
run is slow under TCG, the guest emulated twice (two boots and apk, about
five minutes). Point it at the ISO:

    scripts/hv-distro-test.py --iso alpine-virt-3.24.2-x86_64.iso

Needs xorriso, ssh and ssh-keygen on the host. `--cmdline-extra` adds to the
guest's command line: under TCG the guest cannot calibrate its TSC against
the PIT (an exit costs more than its loop allows) and stays on jiffies and a
periodic tick; `--cmdline-extra "tsc_early_khz=<the host's TSC kHz>
tsc=reliable"` puts it on the TSC and high-resolution timers, the PIT in
one-shot mode, as a real CPU does by itself.

Exit code 0 = every check passed.
"""

import argparse
import importlib.util
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))

spec = importlib.util.spec_from_file_location("hvl", os.path.join(HERE, "hv-linux-test.py"))
hvl = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hvl)
pt = hvl.pt

# What Alpine's boot loader gives its kernel, less the console on tty0: the
# serial console, and a PC with neither a local APIC nor ACPI tables.
CMDLINE = "console=ttyS0 nolapic acpi=off modules=loop,squashfs,sd-mod,usb-storage"
PROMPT = "localhost:~#"
GUEST_IP = "10.0.100.2"
SSH_PORT = 2222


def extract(iso, tmp):
    """The kernel and the initramfs, out of the ISO where Alpine's boot
    loader finds them."""
    out = {}
    for name in ("vmlinuz-virt", "initramfs-virt"):
        dst = os.path.join(tmp, name)
        subprocess.run(["xorriso", "-osirrox", "on", "-indev", iso, "-extract", "/boot/" + name, dst],
                       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        os.chmod(dst, 0o644)
        out[name] = dst
    return out


def run(args):
    tmp = tempfile.mkdtemp(prefix="nos-hvdistro-")
    files = extract(args.iso, tmp)
    key = os.path.join(tmp, "id_test")
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "hv-distro-test", "-f", key],
                   check=True)
    pubkey = open(key + ".pub").read().strip()

    cmdline = CMDLINE + (" " + args.cmdline_extra if args.cmdline_extra else "")
    x = lambda line, secs=60: "hv exec 0 secs=%d %s" % (secs, line)
    login = ["hv wait 0 secs=600 login:", r"hv send 0 root\n", "hv wait 0 secs=120 " + PROMPT]
    rc = (["insmod /hv.ko", "hv on",
           "hv start /bzImage mem=%d initrd=/initrd disk=/alpine.iso:ro net restart cmdline=%s"
           % (args.mem, cmdline)]
          + login
          + [x("cat /etc/alpine-release; uname -r"),
             x("date -u +%s"),
             x("cat /sys/block/vda/ro; dd if=/dev/zero of=/dev/vda bs=512 count=1; echo dd=$?"),
             # Back when the VM has been built again: the line went with the
             # boot it was typed at, and `hv exec` says so. A `hv wait` for
             # the next login prompt before then could find this boot's.
             x("reboot", 600)]
          + login
          + ["hv list",
             x("ip addr show eth0"),
             x("ping -c 3 10.0.100.1"),
             "ping " + GUEST_IP,
             x("apk add openssh-server", 300),
             x("ssh-keygen -q -t ed25519 -N '' -f /etc/ssh/ssh_host_ed25519_key && echo keygen-ok", 300),
             x("mkdir -p /root/.ssh && echo '%s' > /root/.ssh/authorized_keys && echo key-ok" % pubkey),
             x("/usr/sbin/sshd -o HostKey=/etc/ssh/ssh_host_ed25519_key && echo sshd-ok"),
             "hv forward add %d 0 22" % SSH_PORT,
             "hv list",
             hvl.RC_LAST])

    port = hvl.free_port()
    qemu = ["-device", "virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off",
            "-netdev", "user,id=net0,hostfwd=tcp:127.0.0.1:%d-:%d" % (port, SSH_PORT)]
    boot = argparse.Namespace(bzimage=files["vmlinuz-virt"], initrd=files["initramfs-virt"],
                              root_mib=args.root_mib, deadline=args.deadline)
    t_start = time.time()
    p, log, image = hvl.boot_rc(boot, tmp, rc, extra={"alpine.iso": args.iso}, qemu=qemu)
    t_end = time.time()
    try:
        txt = open(log, errors="replace").read()
        if p is None or p.poll() is not None or not hvl.RC_DONE.search(txt):
            return
        secs = hvl.sections(txt)

        def out(line, occurrence=0):
            return hvl.output_of(secs, line, occurrence) or ""

        start = rc[2]
        pt.check("the VM starts, the ISO read-only and on the switch",
                 re.search(r"vm 0 started on cpu \d+", out(start)) is not None, out(start))
        m = re.search(r'printed "login:", (\d+) ms in', out(login[0]))
        pt.check("Alpine's own kernel and initramfs boot it to a login prompt", m is not None, out(login[0]))
        if m:
            print("  (login prompt %.1f s after the VM started)" % (int(m.group(1)) / 1000.0))
        pt.check("root logs in, typed at the getty", "printed \"%s\"" % PROMPT in out(login[2]),
                 out(login[2]))
        release = out(rc[6])
        pt.check("and it is Alpine, on its virt kernel",
                 re.search(r"^\d+\.\d+\.\d+\s*$", release, re.M) is not None and "-virt" in release, release)
        m = re.search(r"^(\d{9,})\s*$", out(rc[7]), re.M)
        pt.check("its clock is the host's, from the emulated RTC",
                 m is not None and t_start - 300 <= int(m.group(1)) <= t_end + 300,
                 "%s (host %d..%d)" % (out(rc[7]), t_start, t_end))
        ro = out(rc[8])
        pt.check("the ISO is read-only to it: the driver says so, and a write fails",
                 re.search(r"^1\s*$", ro, re.M) is not None and "dd=1" in ro, ro)
        pt.check("reboot resets it, and the VM is built again",
                 "the vm restarted" in out(rc[9]), out(rc[9])[-600:])
        pt.check("to a login prompt again", 'printed "login:"' in out(login[0], 1), out(login[0], 1))
        pt.check("where root logs in again", "printed \"%s\"" % PROMPT in out(login[2], 1), out(login[2], 1))
        pt.check("hv list counts the reboot", re.search(r"vm 0\s+running.*restarts 1\b", out(rc[13])) is not None,
                 out(rc[13]))
        eth = out(rc[14])
        pt.check("its initramfs configured eth0 from the VM's ip=",
                 GUEST_IP in eth and re.search(r"(?i)02:00:00:00:64:02", eth) is not None, eth)
        pt.check("it pings nos, at 10.0.100.1",
                 "3 packets transmitted, 3 packets received" in out(rc[15]), out(rc[15])[-600:])
        pt.check("and nos pings it", "reply from " + GUEST_IP in out(rc[16]), out(rc[16])[-600:])
        pt.check("apk installs openssh-server from the ISO",
                 re.search(r"Installing openssh-server \(", out(rc[17])) is not None
                 and re.search(r"OK: .* packages", out(rc[17])) is not None, out(rc[17])[-600:])
        pt.check("its host key, the authorised key and sshd",
                 "keygen-ok" in out(rc[18]) and "key-ok" in out(rc[19]) and "sshd-ok" in out(rc[20]),
                 out(rc[18]) + out(rc[19]) + out(rc[20]))
        pt.check("hv forward add", "port %d forwarded to vm 0, %s:22" % (SSH_PORT, GUEST_IP) in out(rc[21]),
                 out(rc[21]))

        # From outside: the host's port, QEMU's forward into nos's 2222, nos's
        # relay into the guest's sshd.
        answer = ""
        for attempt in range(5):
            r = subprocess.run(["ssh", "-p", str(port), "-i", key, "-o", "BatchMode=yes",
                                "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
                                "-o", "ConnectTimeout=60", "-o", "IdentityAgent=none",
                                "root@127.0.0.1", "cat /etc/alpine-release; uname -r; id"],
                               capture_output=True, text=True, timeout=300)
            answer = r.stdout + r.stderr
            if r.returncode == 0:
                break
            time.sleep(5)
        pt.check("ssh from outside logs into the guest, through hv forward, as root",
                 "uid=0(root)" in answer and "-virt" in answer, answer[-600:])
    finally:
        if p is not None:
            pt.kill(p)
        if args.keep or pt.failures:
            print("log at " + log)
        else:
            shutil.rmtree(tmp, ignore_errors=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--iso", required=True, help="Alpine's alpine-virt-*-x86_64.iso")
    ap.add_argument("--mem", type=int, default=512, help="guest RAM in MiB")
    ap.add_argument("--cmdline-extra", default="", help="more for the guest's command line")
    ap.add_argument("--root-mib", type=int, default=256, help="nos's root filesystem, MiB")
    ap.add_argument("--deadline", type=int, default=1800, help="seconds to wait for /etc/rc to finish")
    ap.add_argument("--keep", action="store_true", help="keep the serial log")
    args = ap.parse_args()
    for tool in ("xorriso", "ssh", "ssh-keygen"):
        if shutil.which(tool) is None:
            sys.exit("hv-distro-test needs %s" % tool)
    if not os.path.exists(args.iso):
        sys.exit("no %s" % args.iso)
    run(args)
    print()
    if pt.failures:
        print("FAILED: " + ", ".join(pt.failures))
        return 1
    print("hv-distro-test: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
