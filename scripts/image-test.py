#!/usr/bin/env python3
"""The system image poured with nos-install and booted through a server's GRUB.

What a release image has to do on a server, and that no other test does
(docs/install.md). nos-install, run as on a server -- under Linux, as root,
on a block device, from the image mounted -- makes the ext2 nos boots from,
writes the GRUB entry and arms one boot. The entry, run as grub-mkconfig
runs it, finds that partition by its UUID, takes its menu entries from the
image there and boots nos once. nos mounts the partition as its root by the
UUID, runs the image's /etc/rc, whose sshd makes a host key and lets in the
key nos-install was given; and no UDP shell answers, since none was asked
for. The boot after that is the machine's own entry again: GRUB cleared the
flag. Then nos-install update over what nos has written: the kernel and its
modules replaced and the old ones kept for a rollback, /etc and the host key
kept, and the kernel parameters it is given -- the UDP shell -- taken; and
status, rollback and uninstall.

The disk: an MBR with a "host" partition holding GRUB as a server's /boot
would -- its default entry stands in for the machine's own OS and says so on
the serial line -- and a blank one for nos, on NVMe as on the Hetzner boxes.
The Linux side (losetup, mkfs, mount, nos-install) runs under sudo on Linux,
and in the nos-builder image, --privileged, elsewhere; QEMU runs here.

    scripts/image-test.py [--image nos-x86_64.squashfs] [--keep]

Build the image first: make image, or scripts/build-image.sh.
"""

import argparse
import os
import platform
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

MiB = 1024 * 1024
DISK_SIZE = 384 * MiB
HOST_START, HOST_SIZE = 1 * MiB, 64 * MiB
NOS_START, NOS_SIZE = 65 * MiB, 256 * MiB

# The UDP shell's framing (scripts/udpsh.py)
SHELL_MAGIC = 0x4E4F5348
SHELL_HDR = struct.Struct("!IIHHHH")
SHELL_LAST = 1
SHELL_PORT = 9000

BOOT_TIMEOUT = 600
HOST_MARKER = "IMAGE-TEST: the host's own entry"

failures = []


def check(name, ok, detail=""):
    print(("PASS: " if ok else "FAIL: ") + name + (" -- " + str(detail) if detail and not ok else ""), flush=True)
    if not ok:
        failures.append(name)
    return ok


def free_port(kind):
    s = socket.socket(socket.AF_INET, kind)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


# The Linux side: the disk's partitions on loop devices, as a server's would
# be block devices. Each phase is one script; $T is the scratch directory,
# $IMAGE the image in it, and every loop device is let go of on the way out.
PRELUDE = r"""
set -euo pipefail
shopt -s inherit_errexit
cd "$T"
LOOPS=()
MOUNTS=()
release() {
    local i
    for (( i = ${#MOUNTS[@]} - 1; i >= 0; i-- )); do umount "${MOUNTS[i]}" 2>/dev/null || true; done
    for i in ${LOOPS[@]+"${LOOPS[@]}"}; do losetup -d "$i" 2>/dev/null || true; done
}
trap release EXIT
# attach VAR OFFSET SIZE: a loop device for that stretch of the disk, named
# in VAR -- set here, not printed for a $(...), whose subshell would keep
# what release() has to let go of
attach() {
    local dev
    dev=$(losetup -f --show -o "$2" --sizelimit "$3" disk.raw)
    [ -b "$dev" ] || { echo "no loop device for disk.raw at $2" >&2; exit 1; }
    LOOPS+=("$dev")
    printf -v "$1" %s "$dev"
}
mount_at() {
    mkdir -p "$2"
    mount ${3:+-o $3} "$1" "$2"
    MOUNTS+=("$2")
}
# The last mount_at undone
unmount_last() {
    umount "${MOUNTS[-1]}"
    unset 'MOUNTS[-1]'
}
# nos-install as a server runs it: from the image, mounted
nos_install() {
    NOS_GRUB_D="$T/grub.d" NOS_UPDATE_GRUB=true NOS_ALLOW_ANY_DISK=1 "$T/img/nos-install" "$@"
}
mount_at "$IMAGE" img loop,ro
cp img/VERSION img-version.out
"""

PREPARE = r"""
truncate -s "$DISK_SIZE" disk.raw
parted -s disk.raw 2>/dev/null mklabel msdos \
    mkpart primary ext2 "$((HOST_START / 512))s" "$(((HOST_START + HOST_SIZE) / 512 - 1))s" \
    mkpart primary ext2 "$((NOS_START / 512))s" "$(((NOS_START + NOS_SIZE) / 512 - 1))s"
attach HOST "$HOST_START" "$HOST_SIZE"
attach NOS "$NOS_START" "$NOS_SIZE"

mkfs.ext2 -q -F -L host "$HOST"
mount_at "$HOST" host
mkdir -p host/boot/grub grub.d
cp -r /usr/lib/grub/i386-pc host/boot/grub/

nos_install install "$NOS" --ssh-key id_test.pub --boot-once > install.out 2>&1 ||
    { cat install.out; exit 1; }
cat install.out

# grub.cfg as grub-mkconfig makes it: the header, the machine's own entry
# first, then what /etc/grub.d/42_nos prints
{
    cat <<'EOF'
serial --unit=0 --speed=115200
terminal_input serial console
terminal_output serial console
set default=0
set timeout=0
menuentry "host" --id host {
    echo "IMAGE-TEST: the host's own entry"
    halt
}
EOF
    sh grub.d/42_nos
} > host/boot/grub/grub.cfg
cp host/boot/grub/grub.cfg grub.cfg.made
unmount_last

grub-mkimage -O i386-pc -o core.img -p "(hd0,msdos1)/boot/grub" biosdisk part_msdos ext2 normal
dd if=/usr/lib/grub/i386-pc/boot.img of=disk.raw bs=440 count=1 conv=notrunc status=none
dd if=core.img of=disk.raw bs=512 seek=1 conv=notrunc status=none
"""

# After nos has run once from the partition, and the host's entry once
UPDATE = r"""
attach NOS "$NOS_START" "$NOS_SIZE"
e2fsck -fn "$NOS" > fsck1.out 2>&1 && echo "fsck1: clean" || { cat fsck1.out; echo "fsck1: DAMAGED"; }
mount_at "$NOS" part ro
grub-editenv part/grubenv list > grubenv1.out
sha256sum part/etc/ssh/ssh_host_ed25519_key | cut -d' ' -f1 > hostkey1.sum || echo none > hostkey1.sum
unmount_last

# no partition named: the GRUB entry names it
nos_install update --udpshell --boot-once > update.out 2>&1 || { cat update.out; exit 1; }
cat update.out

mount_at "$NOS" part ro
grub-editenv part/grubenv list > grubenv2.out
sha256sum part/etc/ssh/ssh_host_ed25519_key | cut -d' ' -f1 > hostkey2.sum
ls part/boot part/lib > layout2.out
cat part/VERSION > version2.out
"""

FINAL = r"""
attach NOS "$NOS_START" "$NOS_SIZE"
e2fsck -fn "$NOS" > fsck2.out 2>&1 && echo "fsck2: clean" || { cat fsck2.out; echo "fsck2: DAMAGED"; }
nos_install status > status.out 2>&1 || { cat status.out; exit 1; }
cat status.out
nos_install rollback > rollback.out 2>&1 || { cat rollback.out; exit 1; }
cat rollback.out
nos_install uninstall > uninstall.out 2>&1 || { cat uninstall.out; exit 1; }
cat uninstall.out
[ ! -e grub.d/42_nos ] && echo "uninstalled: yes"
"""


class Linux:
    """Runs a phase where losetup and mount work: here under sudo on Linux,
    in the builder image elsewhere."""

    def __init__(self, tmp):
        self.tmp = tmp
        self.native = platform.system() == "Linux"

    def run(self, name, script):
        env = {"DISK_SIZE": str(DISK_SIZE), "HOST_START": str(HOST_START), "HOST_SIZE": str(HOST_SIZE),
               "NOS_START": str(NOS_START), "NOS_SIZE": str(NOS_SIZE)}
        if self.native:
            env.update({"T": self.tmp, "IMAGE": os.path.join(self.tmp, "image.squashfs")})
            argv = ["sudo", "env"] + ["%s=%s" % kv for kv in env.items()] + ["bash", "-c", PRELUDE + script]
        else:
            env.update({"T": "/t", "IMAGE": "/t/image.squashfs"})
            argv = ["docker", "run", "--platform", "linux/amd64", "--rm", "--privileged", "-v", "%s:/t" % self.tmp]
            for kv in env.items():
                argv += ["-e", "%s=%s" % kv]
            argv += ["nos-builder", "bash", "-c", PRELUDE + script]
        print("--- %s" % name, flush=True)
        p = subprocess.run(argv, capture_output=True, text=True)
        if self.native:
            # What root made there is QEMU's to write to, and ours to remove
            subprocess.run(["sudo", "chown", "-R", "%d:%d" % (os.getuid(), os.getgid()), self.tmp])
        out = p.stdout + p.stderr
        for line in out.splitlines():
            print("  " + line, flush=True)
        return p.returncode == 0, out

    def read(self, name):
        path = os.path.join(self.tmp, name)
        return open(path, errors="replace").read() if os.path.exists(path) else ""

    def cleanup(self):
        shutil.rmtree(self.tmp, ignore_errors=True)


class Machine:
    def __init__(self, tmp, key):
        self.tmp = tmp
        self.key = key
        self.ssh_port = free_port(socket.SOCK_STREAM)
        self.udp_port = free_port(socket.SOCK_DGRAM)
        self.qemu = None
        self.log = None

    def boot(self, name):
        self.log = os.path.join(self.tmp, "serial-%s.log" % name)
        argv = ["qemu-system-x86_64", "-display", "none", "-no-reboot", "-m", "1G", "-smp", "2",
                "-drive", "file=%s,format=raw,if=none,id=d0" % os.path.join(self.tmp, "disk.raw"),
                "-device", "nvme,drive=d0,serial=nos0,bootindex=0",
                "-netdev", "user,id=n0,hostfwd=tcp:127.0.0.1:%d-:22,hostfwd=udp:127.0.0.1:%d-:%d"
                % (self.ssh_port, self.udp_port, SHELL_PORT),
                "-device", "virtio-net-pci,netdev=n0,disable-legacy=on,disable-modern=off",
                "-device", "virtio-rng-pci", "-serial", "file:" + self.log]
        if os.path.exists("/dev/kvm") and os.access("/dev/kvm", os.W_OK):
            argv += ["-enable-kvm", "-cpu", "host"]
        else:
            argv += ["-cpu", "max"]
        self.qemu = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    def text(self):
        return open(self.log, errors="replace").read() if self.log and os.path.exists(self.log) else ""

    def wait_log(self, marker, timeout):
        start = time.time()
        while time.time() - start < timeout:
            text = self.text()
            if marker in text:
                return True
            if "PANIC" in text:
                print(text[-3000:], flush=True)
                return False
            if self.qemu.poll() is not None:
                return marker in self.text()
            time.sleep(0.5)
        return False

    def wait_exit(self, timeout):
        try:
            self.qemu.wait(timeout)
            return True
        except subprocess.TimeoutExpired:
            return False

    def stop(self):
        if self.qemu is not None and self.qemu.poll() is None:
            self.qemu.send_signal(signal.SIGTERM)
            try:
                self.qemu.wait(10)
            except subprocess.TimeoutExpired:
                self.qemu.kill()
        self.qemu = None

    def ssh(self, args, timeout=120):
        cmd = ["ssh", "-p", str(self.ssh_port), "-i", self.key, "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes",
               "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null", "-o", "ConnectTimeout=20",
               "-o", "LogLevel=ERROR", "root@127.0.0.1"] + args
        try:
            p = subprocess.run(cmd, capture_output=True, timeout=timeout)
        except subprocess.TimeoutExpired:
            return None, ""
        out = p.stdout.decode(errors="replace")
        print("  ssh %s -> %s%s" % (" ".join(args), p.returncode,
                                   "".join("\n    " + line for line in out.splitlines()[:12])), flush=True)
        return p.returncode, out

    def wait_ssh(self, timeout):
        start = time.time()
        while time.time() - start < timeout:
            rc, out = self.ssh(["version"], timeout=60)
            if rc == 0 and out.startswith("nos "):
                return out.strip()
            if self.qemu.poll() is not None:
                return None
            time.sleep(3)
        return None

    def host_key(self):
        p = subprocess.run(["ssh-keyscan", "-p", str(self.ssh_port), "-t", "ed25519", "127.0.0.1"],
                           capture_output=True, text=True, timeout=60)
        keys = [line.split(None, 1)[1] for line in p.stdout.splitlines() if line and not line.startswith("#")]
        return keys[0] if keys else None

    def udp_shell(self, cmd, timeout=5):
        """What the UDP shell answers, or None when nothing does."""
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.settimeout(timeout)
        body = cmd.encode()
        try:
            sock.sendto(SHELL_HDR.pack(SHELL_MAGIC, 1, 0, 0, len(body), 0) + body, ("127.0.0.1", self.udp_port))
            chunks, last = {}, None
            while True:
                data, _ = sock.recvfrom(4096)
                magic, seq, idx, flags, length, _ = SHELL_HDR.unpack(data[:SHELL_HDR.size])
                if magic != SHELL_MAGIC or seq != 1:
                    continue
                chunks[idx] = data[SHELL_HDR.size:SHELL_HDR.size + length]
                if flags & SHELL_LAST:
                    last = idx
                if last is not None and len(chunks) == last + 1:
                    return b"".join(chunks[i] for i in range(last + 1)).decode(errors="replace")
        except socket.timeout:
            return None
        finally:
            sock.close()


def expected_version(text):
    fields = dict(line.split("=", 1) for line in text.splitlines() if "=" in line)
    return "nos %s (%s)" % (fields.get("version", "?"), fields.get("revision", "?"))


def run(tmp, image):
    linux = Linux(tmp)
    shutil.copy(image, os.path.join(tmp, "image.squashfs"))
    key = os.path.join(tmp, "id_test")
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "image-test", "-f", key], check=True)

    ok, out = linux.run("nos-install install", PREPARE)
    if not check("nos-install install", ok):
        return linux
    want = expected_version(linux.read("img-version.out"))
    check("install says it puts no key nowhere: it was given one", "no --ssh-key" not in out, out)
    snippet = linux.read("grub.cfg.made")
    check("the GRUB entry names the partition by UUID", "search --no-floppy --fs-uuid --set=nos_root" in snippet)
    m = Machine(tmp, key)

    # Boot 1: armed by install --boot-once
    m.boot("1")
    try:
        check("GRUB boots nos once, from the image's entry", m.wait_log("Loading nos...", 180), m.text()[-2000:])
        check("nos boots", m.wait_log("boot: complete", BOOT_TIMEOUT), m.text()[-3000:])
        text = m.text()
        check("its root is the partition, by UUID, read-write",
              "MountRootFs: mounted ext2 on /" in text and "(rw)" in text, text[-3000:])
        version = m.wait_ssh(300)
        check("sshd from the image's /etc/rc lets in the key install was given", version is not None)
        first_key = m.host_key()
        check("sshd made a host key", first_key is not None)
        if version is not None:
            check("the kernel is the image's", version == want, "%r, the image's VERSION says %r" % (version, want))
            rc, out = m.ssh(["cat", "/VERSION"])
            check("so is the VERSION on its root", rc == 0 and expected_version(out) == want, out)
            rc, out = m.ssh(["cat", "/proc/cmdline"])
            check("the command line has no udpshell", rc == 0 and "udpshell" not in out and "root=UUID=" in out, out)
        check("no UDP shell answers", m.udp_shell("version") is None)
        m.ssh(["reboot"], timeout=60)
        check("nos reboots", m.wait_exit(120))
    finally:
        m.stop()

    # Boot 2: GRUB cleared the flag, so the machine's own entry
    m.boot("2")
    try:
        check("the boot after it is the machine's own", m.wait_log(HOST_MARKER, 180) and
              "Loading nos" not in m.text(), m.text()[-2000:])
    finally:
        m.stop()

    ok, out = linux.run("nos-install update", UPDATE)
    if not check("nos-install update", ok):
        return linux
    check("e2fsck finds the partition clean after nos ran on it", "fsck1: clean" in out, out)
    check("GRUB cleared the one-shot flag", "nos_next=" in linux.read("grubenv1.out").splitlines(),
          linux.read("grubenv1.out"))
    check("update leaves the GRUB entry as it was: the partition is the same", "wrote" not in out, out)
    check("the host key nos made is on the partition", linux.read("hostkey1.sum").strip() not in ("", "none"))
    check("update keeps the host key", linux.read("hostkey1.sum") == linux.read("hostkey2.sum"))
    layout = linux.read("layout2.out")
    check("update keeps the old kernel and modules for a rollback",
          "kernel64.elf.prev" in layout and "modules.prev" in layout, layout)
    env2 = linux.read("grubenv2.out")
    check("update takes the UDP shell it was given, and arms a boot",
          "nos_cmdline=udpshell=9000" in env2 and "nos_next=nos" in env2, env2)

    # Boot 3: the update, with the UDP shell asked for
    m.boot("3")
    try:
        check("nos boots again", m.wait_log("boot: complete", BOOT_TIMEOUT), m.text()[-3000:])
        version = m.wait_ssh(300)
        check("sshd lets the key in again, to the same kernel", version == want, "%r != %r" % (version, want))
        check("with the host key it made the first time", m.host_key() == first_key)
        out = m.udp_shell("version", timeout=10)
        check("the UDP shell answers, now that it was asked for", out is not None and out.startswith("nos "), out)
        m.ssh(["poweroff"], timeout=60)
        check("nos powers off", m.wait_exit(120))
    finally:
        m.stop()

    ok, out = linux.run("status, rollback, uninstall", FINAL)
    check("status, rollback and uninstall", ok, out)
    check("e2fsck finds the partition clean after the second run", "fsck2: clean" in out, out)
    status = linux.read("status.out")
    check("status says what is installed and what was", "installed:   nos " in status and
          "previous:    nos " in status, status)
    check("uninstall takes the GRUB entry away", "uninstalled: yes" in out, out)
    return linux


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--image", default=os.path.join(ROOT, "nos-x86_64.squashfs"))
    p.add_argument("--keep", action="store_true", help="keep the scratch directory: disk, logs, outputs")
    args = p.parse_args()
    if not os.path.exists(args.image):
        sys.exit("no %s: make image, or scripts/build-image.sh" % args.image)
    for tool in ("qemu-system-x86_64", "ssh", "ssh-keygen", "ssh-keyscan"):
        if shutil.which(tool) is None:
            sys.exit("image-test needs %s" % tool)

    tmp = tempfile.mkdtemp(prefix="image-test.")
    print("image-test: %s, scratch in %s" % (args.image, tmp), flush=True)
    linux = None
    try:
        linux = run(tmp, args.image)
    finally:
        if args.keep or failures:
            print("image-test: kept %s" % tmp, flush=True)
        elif linux is not None:
            linux.cleanup()
    if failures:
        print("image-test: %d FAILED: %s" % (len(failures), ", ".join(failures)), flush=True)
        sys.exit(1)
    print("image-test: all passed", flush=True)


if __name__ == "__main__":
    main()
