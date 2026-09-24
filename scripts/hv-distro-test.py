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

And with `--debian`, a mainstream one: Debian's `nocloud` cloud image,
systemd and initramfs-tools, its kernel and initrd read out of the image's
own /boot, its root partition the guest's disk and written to:

  - systemd-firstboot is given what it would ask the console for -- the root
    password, the locale, the keymap, the timezone -- as credentials on the
    kernel command line (the image's root is "!unprovisioned" until then);
    root logs in with that password
  - it is Debian on its own kernel, `systemctl is-system-running` says
    running, and no unit failed
  - its root is /dev/vda1, ext4, read-write; its clock is the host's
  - on the switch, given its port's address by hand (the image configures
    no ethernet interface itself), its virtio-net driver reaches nos and
    nos reaches it
  - a file written and synced there is still there after `reboot`, the VM
    built again: what the guest wrote went through nos's ext2 to its file

Manual, like hv-linux-test: the images are downloads CI does not make, and
the runs are slow under TCG, the guest emulated twice (Alpine: two boots and
apk, about five minutes; Debian: two boots, about as long). Point it at
them:

    scripts/hv-distro-test.py --iso alpine-virt-3.24.2-x86_64.iso
    scripts/hv-distro-test.py --debian debian-13-nocloud-amd64.raw

(the Debian image as raw: `qemu-img convert -O raw` the .qcow2). Needs
xorriso, ssh and ssh-keygen on the host for Alpine, sfdisk and debugfs for
Debian. `--cmdline-extra` adds to the
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


# How long the guest idles while its clock is measured against nos's, and
# how far the two may part: a guest whose idle vCPU lost ticks counted 40%.
CLOCK_IDLE_S = 40
CLOCK_TOLERANCE = 0.1


def clock_lines(vm, x):
    """rc lines that read the guest's uptime and nos's around an idle
    stretch: what `clock_rate` makes a ratio of."""
    return [x("cat /proc/uptime"), "uptime",
            "hv wait %d secs=%d nos-never-prints-this" % (vm, CLOCK_IDLE_S),
            x("cat /proc/uptime"), "uptime"]


def clock_rate(out, lines):
    """The guest's seconds per nos second over the idle stretch, or None."""
    num = lambda text: [float(v) for v in re.findall(r"(?m)^(\d+\.\d+)\b", text)]
    try:
        g0, g1 = num(out(lines[0], 0))[0], num(out(lines[0], 1))[0]
        h0, h1 = num(out(lines[1], 0))[0], num(out(lines[1], 1))[0]
    except IndexError:
        return None
    return (g1 - g0) / (h1 - h0) if h1 > h0 else None


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


def alpine(args):
    tmp = tempfile.mkdtemp(prefix="nos-hvdistro-")
    files = extract(args.iso, tmp)
    key = os.path.join(tmp, "id_test")
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "hv-distro-test", "-f", key],
                   check=True)
    pubkey = open(key + ".pub").read().strip()

    cmdline = CMDLINE + (" " + args.cmdline_extra if args.cmdline_extra else "")
    x = lambda line, secs=60: "hv exec 0 secs=%d %s" % (secs, line)
    eth_line = x("ip addr show eth0")
    ping_line = x("ping -c 3 10.0.100.1")
    apk_line = x("apk add openssh-server", 300)
    keygen_line = x("ssh-keygen -q -t ed25519 -N '' -f /etc/ssh/ssh_host_ed25519_key && echo keygen-ok", 300)
    key_line = x("mkdir -p /root/.ssh && echo '%s' > /root/.ssh/authorized_keys && echo key-ok" % pubkey)
    sshd_line = x("/usr/sbin/sshd -o HostKey=/etc/ssh/ssh_host_ed25519_key && echo sshd-ok")
    forward_line = "hv forward add %d 0 22" % SSH_PORT
    def login(boot):
        return ["hv wait 0 secs=600 boot=%d login:" % boot, r"hv send 0 root\n", "hv wait 0 secs=120 " + PROMPT]
    rc = (["insmod /hv.ko", "hv on",
           "hv start /bzImage mem=%d initrd=/initrd disk=/alpine.iso:ro net restart cmdline=%s"
           % (args.mem, cmdline)]
          + login(0)
          + [x("cat /etc/alpine-release; uname -r"),
             x("date -u +%s"),
             x("cat /sys/block/vda/ro; dd if=/dev/zero of=/dev/vda bs=512 count=1; echo dd=$?")]
          + clock_lines(0, x)
          + [# The shell may print its prompt before the system goes down:
             # the boot after this one is what `boot=1` waits for.
             r"hv send 0 reboot\n"]
          + login(1)
          + ["hv list",
             eth_line,
             ping_line,
             "ping " + GUEST_IP,
             apk_line,
             keygen_line,
             key_line,
             sshd_line,
             forward_line,
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
        first, second = login(0), login(1)
        m = re.search(r'printed "login:", (\d+) ms in', out(first[0]))
        pt.check("Alpine's own kernel and initramfs boot it to a login prompt", m is not None, out(first[0]))
        if m:
            print("  (login prompt %.1f s after the VM started)" % (int(m.group(1)) / 1000.0))
        pt.check("root logs in, typed at the getty", "printed \"%s\"" % PROMPT in out(first[2]),
                 out(first[2]))
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
        rate = clock_rate(out, clock_lines(0, x))
        pt.check("idle, its clock keeps the host's time (no tick lost)",
                 rate is not None and abs(rate - 1) <= CLOCK_TOLERANCE, "rate %s" % rate)
        if rate is not None:
            print("  (guest seconds per host second, idle: %.3f)" % rate)
        pt.check("reboot resets it, and the VM boots again to a login prompt",
                 'printed "login:" in boot 1' in out(second[0]), out(second[0]))
        pt.check("where root logs in again", "printed \"%s\"" % PROMPT in out(second[2], 1), out(second[2], 1))
        pt.check("hv list counts the reboot", re.search(r"vm 0\s+running.*restarts 1\b", out("hv list")) is not None,
                 out("hv list"))
        eth = out(eth_line)
        pt.check("its initramfs configured eth0 from the VM's ip=",
                 GUEST_IP in eth and re.search(r"(?i)02:00:00:00:64:02", eth) is not None, eth)
        pt.check("it pings nos, at 10.0.100.1",
                 "3 packets transmitted, 3 packets received" in out(ping_line), out(ping_line)[-600:])
        pt.check("and nos pings it", "reply from " + GUEST_IP in out("ping " + GUEST_IP), out("ping " + GUEST_IP)[-600:])
        pt.check("apk installs openssh-server from the ISO",
                 re.search(r"Installing openssh-server \(", out(apk_line)) is not None
                 and re.search(r"OK: .* packages", out(apk_line)) is not None, out(apk_line)[-600:])
        pt.check("its host key, the authorised key and sshd",
                 "keygen-ok" in out(keygen_line) and "key-ok" in out(key_line) and "sshd-ok" in out(sshd_line),
                 out(keygen_line) + out(key_line) + out(sshd_line))
        pt.check("hv forward add", "port %d forwarded to vm 0, %s:22" % (SSH_PORT, GUEST_IP) in out(forward_line),
                 out(forward_line))

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


# Debian's cloud image: its root partition, found by its GPT type, and the
# kernel and initrd in its /boot, which are the only ones there.
DEBIAN_ROOT_TYPE = "4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709"   # Linux x86-64 root
# The image's root is locked ("!unprovisioned") until systemd-firstboot sets
# it, which asks at the console for what it is not given: the password, the
# locale, the keymap and the timezone, handed to it as credentials on the
# kernel command line, as systemd provisions a machine with no one at it.
DEBIAN_PASSWORD = "nos"
DEBIAN_CMDLINE = ("root=/dev/vda1 ro console=ttyS0 nolapic acpi=off"
                  " systemd.set_credential=passwd.plaintext-password.root:" + DEBIAN_PASSWORD +
                  " systemd.set_credential=firstboot.locale:C.UTF-8"
                  " systemd.set_credential=firstboot.keymap:us"
                  " systemd.set_credential=firstboot.timezone:UTC")
DEBIAN_PROMPT = "root@localhost:~#"


def debian_boot_files(image, tmp):
    """The kernel and the initrd out of the image's root partition: its
    offset from sfdisk, the partition cut out sparse, and debugfs to read
    /boot."""
    import json
    table = json.loads(subprocess.run(["sfdisk", "-J", image], check=True, capture_output=True,
                                      text=True).stdout)["partitiontable"]
    root = next(p for p in table["partitions"] if p["type"].upper() == DEBIAN_ROOT_TYPE)
    part = os.path.join(tmp, "root.part")
    sector = table.get("sectorsize", 512)
    subprocess.run(["dd", "if=" + image, "of=" + part, "bs=%d" % sector, "skip=%d" % root["start"],
                    "count=%d" % root["size"], "conv=sparse", "status=none"], check=True)
    listing = subprocess.run(["debugfs", "-R", "ls /boot", part], check=True, capture_output=True,
                             text=True).stdout
    names = listing.split()
    out = {}
    for prefix in ("vmlinuz-", "initrd.img-"):
        name = next(n for n in names if n.startswith(prefix))
        dst = os.path.join(tmp, prefix.rstrip("-."))
        subprocess.run(["debugfs", "-R", "dump /boot/%s %s" % (name, dst), part], check=True,
                       capture_output=True)
        out[prefix] = dst
    os.unlink(part)
    return out["vmlinuz-"], out["initrd.img-"]


def debian(args):
    """Debian's cloud image, as it ships: systemd, initramfs-tools, and its
    root on the guest's disk, written to -- and still there after a reboot."""
    tmp = tempfile.mkdtemp(prefix="nos-hvdebian-")
    vmlinuz, initrd = debian_boot_files(args.debian, tmp)
    rootdir = os.path.join(tmp, "rootdir")
    os.makedirs(rootdir)
    # A copy, holes and all: the guest writes to it, and it is 3 GiB.
    subprocess.run(["cp", "--sparse=always", args.debian, os.path.join(rootdir, "debian.raw")], check=True)
    marker = "nos-was-here-%d" % os.getpid()

    x = lambda line, secs=60: "hv exec 0 secs=%d %s" % (secs, line)
    marker_line = x("echo %s > /root/nos.txt && sync && cat /root/nos.txt" % marker)
    net_line = x("dev=$(ls /sys/class/net | grep -v '^lo$' | head -1); ip addr add %s/24 dev $dev"
                 " && ip link set $dev up && ping -c 3 10.0.100.1" % GUEST_IP)
    kept_line = x("cat /root/nos.txt")
    def login(boot):
        return ["hv wait 0 secs=600 boot=%d login:" % boot, r"hv send 0 root\n", "hv wait 0 secs=120 Password:",
                r"hv send 0 %s\n" % DEBIAN_PASSWORD, "hv wait 0 secs=120 " + DEBIAN_PROMPT]
    rc = (["insmod /hv.ko", "hv on",
           "hv start /bzImage mem=%d initrd=/initrd disk=/debian.raw net restart cmdline=%s"
           % (args.debian_mem, DEBIAN_CMDLINE)]
          + login(0)
          + [x("cat /etc/debian_version; uname -r"),
             x("systemctl is-system-running --wait", 300),
             x("systemctl --failed --no-legend --no-pager | wc -l"),
             x("findmnt -rno SOURCE,FSTYPE,OPTIONS /"),
             x("date -u +%s")]
          + clock_lines(0, x)
          + [marker_line,
             # The image configures no ethernet interface (networkd has no
             # .network for one, and no cloud-init to write one): its own
             # virtio-net driver, given the port's address by hand.
             net_line,
             "ping " + GUEST_IP,
             # systemd's reboot returns at once and bash prints its prompt;
             # the boot after it is what `boot=1` waits for.
             r"hv send 0 reboot\n"]
          + login(1)
          + [kept_line,
             "hv list",
             hvl.RC_LAST])
    boot = argparse.Namespace(bzimage=vmlinuz, initrd=initrd, root_mib=args.debian_root_mib,
                              deadline=args.deadline)
    t_start = time.time()
    p, log, image = hvl.boot_rc(boot, tmp, rc)
    t_end = time.time()
    try:
        txt = open(log, errors="replace").read()
        if p is None or p.poll() is not None or not hvl.RC_DONE.search(txt):
            return
        secs = hvl.sections(txt)

        def out(line, occurrence=0):
            return hvl.output_of(secs, line, occurrence) or ""

        first, second = login(0), login(1)
        m = re.search(r'printed "login:", (\d+) ms in', out(first[0]))
        pt.check("Debian's own kernel and initrd boot it, systemd, to a login prompt", m is not None,
                 out(first[0]))
        if m:
            print("  (login prompt %.1f s after the VM started)" % (int(m.group(1)) / 1000.0))
        pt.check("root logs in, with the password systemd-firstboot was given",
                 "printed \"%s\"" % DEBIAN_PROMPT in out(first[4]), out(first[2]) + out(first[4]))
        pt.check("and it is Debian, on its own kernel",
                 re.search(r"^\d+\.\d+\s*$", out(rc[8]), re.M) is not None and "deb" in out(rc[8]), out(rc[8]))
        pt.check("systemd says the system is running", re.search(r"^running\s*$", out(rc[9]), re.M) is not None,
                 out(rc[9]))
        pt.check("with no unit failed", re.search(r"^0\s*$", out(rc[10]), re.M) is not None, out(rc[10]))
        pt.check("its root is the guest's disk, ext4, read-write",
                 re.search(r"^/dev/vda1 ext4 rw", out(rc[11]), re.M) is not None, out(rc[11]))
        m = re.search(r"^(\d{9,})\s*$", out(rc[12]), re.M)
        pt.check("its clock is the host's", m is not None and t_start - 300 <= int(m.group(1)) <= t_end + 300,
                 out(rc[12]))
        rate = clock_rate(out, clock_lines(0, x))
        pt.check("idle, its clock keeps the host's time (no tick lost)",
                 rate is not None and abs(rate - 1) <= CLOCK_TOLERANCE, "rate %s" % rate)
        if rate is not None:
            print("  (guest seconds per host second, idle: %.3f)" % rate)
        pt.check("a file written to its root and synced", re.search(r"^%s\s*$" % marker, out(marker_line), re.M)
                 is not None, out(marker_line))
        pt.check("reboot resets it, and the VM boots again", 'printed "login:" in boot 1' in out(second[0]),
                 out(second[0]))
        pt.check("root logs in again", "printed \"%s\"" % DEBIAN_PROMPT in out(second[4], 1), out(second[4], 1))
        pt.check("on the switch, its virtio-net driver reaches nos",
                 re.search(r"3 packets transmitted, 3 (packets )?received", out(net_line)) is not None,
                 out(net_line)[-600:])
        pt.check("and nos reaches it", "reply from " + GUEST_IP in out("ping " + GUEST_IP),
                 out("ping " + GUEST_IP)[-600:])
        pt.check("and the file is still there: the disk kept what the guest wrote",
                 re.search(r"^%s\s*$" % marker, out(kept_line), re.M) is not None, out(kept_line))
        pt.check("hv list counts the reboot", re.search(r"vm 0\s+running.*restarts 1\b", out("hv list"))
                 is not None, out("hv list"))
    finally:
        if p is not None:
            pt.kill(p)
        if args.keep or pt.failures:
            print("log at " + log)
        else:
            shutil.rmtree(tmp, ignore_errors=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--iso", help="Alpine's alpine-virt-*-x86_64.iso")
    ap.add_argument("--debian", help="Debian's debian-*-nocloud-amd64.raw (qemu-img convert -O raw a .qcow2)")
    ap.add_argument("--mem", type=int, default=512, help="the Alpine guest's RAM in MiB")
    ap.add_argument("--debian-mem", type=int, default=768, help="the Debian guest's RAM in MiB")
    ap.add_argument("--cmdline-extra", default="", help="more for the Alpine guest's command line")
    ap.add_argument("--root-mib", type=int, default=256, help="nos's root filesystem for Alpine, MiB")
    ap.add_argument("--debian-root-mib", type=int, default=3700, help="nos's root filesystem for Debian, MiB")
    ap.add_argument("--deadline", type=int, default=1800, help="seconds to wait for /etc/rc to finish")
    ap.add_argument("--keep", action="store_true", help="keep the serial log")
    args = ap.parse_args()
    if not args.iso and not args.debian:
        sys.exit("hv-distro-test: --iso <alpine-virt.iso>, --debian <debian-nocloud.raw>, or both")
    tools = []
    if args.iso:
        tools += ["xorriso", "ssh", "ssh-keygen"]
    if args.debian:
        tools += ["sfdisk", "debugfs"]
    for tool in tools:
        if shutil.which(tool) is None:
            sys.exit("hv-distro-test needs %s" % tool)
    for path in (args.iso, args.debian):
        if path and not os.path.exists(path):
            sys.exit("no %s" % path)
    if args.iso:
        alpine(args)
    if args.debian:
        debian(args)
    print()
    if pt.failures:
        print("FAILED: " + ", ".join(pt.failures))
        return 1
    print("hv-distro-test: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
