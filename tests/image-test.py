#!/usr/bin/env python3
"""The system image poured with nos-install and booted through a server's GRUB.

What a release image has to do on a server, and that no other test does
(docs/install.md). nos-install, run as on a server -- under Linux, as root,
on a block device, from the image mounted -- makes the ext2 nos boots from,
writes the GRUB entry and arms one boot; every file of the image must then
be on the partition as it is in the image, and an image holding a file
nos-install does not know where to put must be refused before anything is
written, rather than poured without it. The entry, run as grub-mkconfig
runs it, finds that partition by its UUID, takes its menu entries from the
image there and boots nos once. nos mounts the partition as its root by the
UUID, runs the image's /etc/rc, whose sshd makes a host key and lets in the
key nos-install was given; and no UDP shell answers, since none was asked
for. The machine's microcode -- a made-up Intel CPU's, from a made-up
/proc/cpuinfo and firmware directory -- goes onto the partition, GRUB hands
it to the kernel as the module `ucode`, and the kernel finds it and says why
it loads nothing under QEMU. The boot after that is the machine's own entry
again: GRUB cleared the flag. Then nos-install update over what nos has
written: the kernel, its modules and hvboot replaced -- every file of the
image there again -- and the old ones kept for a rollback, with their
VERSION, /etc and the host key kept, and the kernel parameters it is given
-- the UDP shell -- taken. The update is given --panel too: root's password
for the management panel printed once, its scrypt hash alone on the
partition, as the panel checks it, and /etc/rc given the panel's lines --
whose api.ko then serves at the next boot, and whose `api panel create`
makes no panel of an image released without one, and says so. Then status,
rollback and uninstall.

The disk: an MBR with a "host" partition holding GRUB as a server's /boot
would -- its default entry stands in for the machine's own OS and says so on
the serial line -- and a blank one for nos, on virtio-blk. Not on NVMe, as on
the Hetzner boxes: the SeaBIOS of Ubuntu 22.04's QEMU (1.15), which CI runs,
finds an NVMe disk and boots nothing from it -- "Booting from Hard Disk...",
GRUB's core image never read, and nothing on the serial line -- where a newer
one boots it; nos reads both, and the disk's driver is the smoke boot's to
test, not this.
Then a whole disk made nos's own, as an installer or a provider's rescue
system makes one (nos-install disk), with no GRUB of the machine's to touch:
refused, before anything is written, when it holds something and --wipe is
not given, when it is a partition, when a filesystem of it is mounted and
when the root's 10G does not fit; then a GPT -- BIOS boot, EFI System, the
root of the size asked for, the rest a partition with nothing on it -- the
image on the root, and GRUB for BIOS and UEFI. The disk boots nos alone,
under SeaBIOS and under OVMF (by the path firmware boots a disk with no boot
entry), from its own grub.cfg, with the command line kept in its grubenv,
and nos sees the blank partition by its GUID. Status and update find that
root by themselves and leave every GRUB of the machine alone; boot has
nothing to arm, uninstall nothing to take out, and an install over it is
refused.

The Linux side (losetup, mkfs, mount, nos-install) runs under sudo on Linux,
and in the nos-builder image, --privileged, elsewhere; QEMU runs here.

    tests/image-test.py [--image nos-x86_64.squashfs] [--keep]

Build the image first: make image, or scripts/build-image.sh.
"""

import argparse
import base64
import hashlib
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
# The disk of nos's own: room for GRUB's two partitions, a root of 1G and a
# blank one after it, and not for the 10G a root is unless told
WHOLE_SIZE = 3 * 1024 * MiB
WHOLE_ROOT = "1G"
SECTOR = 512
GPT_BIOS_BOOT = "21686148-6449-6E6F-744E-656564454649"
GPT_ESP = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"
GPT_LINUX = "0FC63DAF-8483-4772-8E79-3D69D8477DE4"
STANDALONE_MARK = "# nos: this disk's own GRUB configuration, written by nos-install disk"

# The UDP shell's framing (scripts/udpsh.py)
SHELL_MAGIC = 0x4E4F5348
SHELL_HDR = struct.Struct("!IIHHHH")
SHELL_LAST = 1
SHELL_PORT = 9000

BOOT_TIMEOUT = 600
# Seconds a boot's /etc/rc is given to start the host API after sshd answers.
API_WAIT = 60
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
# nos-install as a server runs it: from the image, mounted -- or from the
# copy of it IMG_DIR names
nos_install() {
    NOS_GRUB_D="$T/grub.d" NOS_UPDATE_GRUB=true NOS_ALLOW_ANY_DISK=1 \
        NOS_CPUINFO="$T/cpuinfo" NOS_UCODE_DIR="$T/intel-ucode" "$T/${IMG_DIR:-img}/nos-install" "$@"
}
# same_as_image DIR WHEN [FILE...]: every file of the image is in DIR as it
# is in the image -- nos-install aside, which stays in it, and the FILEs
# named, which are the machine's by then -- or the ones that are not are
# named. Not from nos-install's list of what goes where: a file it leaves out
# is one that list does not have
same_as_image() {
    local f bad=0 dir=$1 when=$2
    shift 2
    while IFS= read -r f; do
        case " $* " in *" $f "*) continue ;; esac
        cmp -s "img/$f" "$dir/$f" || { echo "$when: not on the partition as in the image: /$f"; bad=1; }
    done < <(cd img && find . -type f ! -path ./nos-install | sed 's|^\./||' | sort)
    [ "$bad" = 1 ] || echo "$when: every file of the image"
}
mount_at "$IMAGE" img loop,ro
cp img/VERSION img-version.out
cat img/lib/panel/bundle > img-bundle.out 2>/dev/null || echo none > img-bundle.out
cat img/lib/licenses/CRYPTOGAMS > img-license.out 2>/dev/null || echo none > img-license.out
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

# An image with a file in it that nos-install does not know is refused,
# before anything is written
cp -r img stray
touch stray/lib/stray
if IMG_DIR=stray nos_install install "$NOS" --ssh-key id_test.pub > stray.out 2>&1; then
    echo "stray: installed"
fi
cat stray.out
[ -n "$(blkid -p -o value -s TYPE "$NOS" || true)" ] || echo "stray: the partition is as it was"
rm -rf stray

nos_install install "$NOS" --ssh-key id_test.pub --boot-once > install.out 2>&1 ||
    { cat install.out; exit 1; }
cat install.out
mount_at "$NOS" part ro
same_as_image part install
unmount_last

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
nos_install update --udpshell --boot-once --panel > update.out 2>&1 || { cat update.out; exit 1; }
cat update.out

mount_at "$NOS" part ro
cp part/etc/rc rc2.out
cat part/etc/api/panel.pass > panelpass2.out 2>/dev/null || echo none > panelpass2.out
grub-editenv part/grubenv list > grubenv2.out
sha256sum part/etc/ssh/ssh_host_ed25519_key | cut -d' ' -f1 > hostkey2.sum
ls part part/boot part/lib > layout2.out
cat part/VERSION > version2.out
# /etc/rc is the machine's after the first install, and --panel gave it its
# lines: those are checked from rc2.out
same_as_image part update etc/rc
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

# A disk of nos's own: the whole of whole.raw on a loop device that has its
# partitions, nos-install run with no 42_nos anywhere -- as on an installer
# or a rescue system -- and an update-grub that fails the run, should one be
# asked for
WHOLE_HELPERS = r"""
attach_disk() {
    local dev
    dev=$(losetup -fP --show "$2")
    [ -b "$dev" ] || { echo "no loop device for $2" >&2; exit 1; }
    LOOPS+=("$dev")
    printf -v "$1" %s "$dev"
}
disk_install() {
    mkdir -p "$T/grub.d.disk"
    NOS_GRUB_D="$T/grub.d.disk" NOS_UPDATE_GRUB=false NOS_ALLOW_ANY_DISK=1 \
        NOS_CPUINFO="$T/cpuinfo" NOS_UCODE_DIR="$T/intel-ucode" "$T/img/nos-install" "$@"
}
first_mib() { dd if="$1" bs=1M count=1 status=none | sha256sum | cut -d' ' -f1; }
blank_mib() { [ -z "$(dd if="$1" bs=1M count=1 status=none | tr -d '\000' | head -c 1)" ]; }
"""

WHOLE_PREPARE = WHOLE_HELPERS + r"""
truncate -s "$WHOLE_SIZE" whole.raw
attach_disk WHOLE whole.raw

# Refused before anything is written: a disk that holds something, without
# --wipe
parted -s "$WHOLE" mklabel msdos mkpart primary ext2 1MiB 64MiB
before=$(first_mib "$WHOLE")
if disk_install disk "$WHOLE" --root-size "$WHOLE_ROOT" --ssh-key id_test.pub > refuse-holds.out 2>&1; then
    echo "holds: installed"
fi
cat refuse-holds.out
[ "$(first_mib "$WHOLE")" != "$before" ] || echo "holds: the disk is as it was"

# ... a partition, not a disk
if disk_install disk "${WHOLE}p1" --wipe > refuse-part.out 2>&1; then
    echo "part: installed"
fi
cat refuse-part.out

# ... a disk with a filesystem of it mounted, --wipe or not
mkfs.ext2 -q -F "${WHOLE}p1"
mount_at "${WHOLE}p1" busy
if disk_install disk "$WHOLE" --wipe --root-size "$WHOLE_ROOT" > refuse-mounted.out 2>&1; then
    echo "mounted: installed"
fi
cat refuse-mounted.out
unmount_last

# ... and a root of the size it is unless told, which the disk has no room for
before=$(first_mib "$WHOLE")
if disk_install disk "$WHOLE" --wipe --ssh-key id_test.pub > refuse-size.out 2>&1; then
    echo "size: installed"
fi
cat refuse-size.out
[ "$(first_mib "$WHOLE")" != "$before" ] || echo "size: the disk is as it was"

disk_install disk "$WHOLE" --wipe --root-size "$WHOLE_ROOT" --ssh-key id_test.pub \
    --netconsole 10.0.2.2:6666 > disk.out 2>&1 || { cat disk.out; exit 1; }
cat disk.out
sfdisk --dump "$WHOLE" > layout.out
cat layout.out
fsck.vfat -n "${WHOLE}p2" > fsckfat.out 2>&1 && echo "esp: clean" || { cat fsckfat.out; echo "esp: DAMAGED"; }
e2fsck -fn "${WHOLE}p3" > fsckroot.out 2>&1 && echo "root: clean" || { cat fsckroot.out; echo "root: DAMAGED"; }
[ -n "$(blkid -p -o value -s TYPE "${WHOLE}p4" || true)" ] || ! blank_mib "${WHOLE}p4" || echo "data: blank"
[ -z "$(dd if="$WHOLE" bs=440 count=1 status=none | tr -d '\000' | head -c 1)" ] || echo "mbr: boot code"
blkid -p -o value -s PART_ENTRY_UUID "${WHOLE}p4" > data-partuuid.out
blkid -p -o value -s UUID "${WHOLE}p3" > root-uuid.out
mount_at "${WHOLE}p2" esp ro
[ ! -f esp/EFI/BOOT/BOOTX64.EFI ] || echo "esp: BOOTX64.EFI"
unmount_last
mount_at "${WHOLE}p3" root ro
head -n 1 root/boot/grub/grub.cfg > grubcfg-first.out
sed -n 's/^set nos_uuid=//p' root/boot/grub/grub.cfg > grubcfg-uuid.out
[ ! -f root/boot/grub/i386-pc/core.img ] || echo "root: GRUB for BIOS"
[ ! -d root/boot/grub/x86_64-efi ] || echo "root: GRUB for UEFI"
grub-editenv root/grubenv list > disk-grubenv.out
echo "keys: $(grep -c '^ssh-ed25519 ' root/etc/ssh/authorized_keys || true)"
same_as_image root disk
unmount_last
echo "grub.d: $(ls -A grub.d.disk | wc -l)"
"""

# After nos has run from it, under BIOS and UEFI
WHOLE_AFTER = WHOLE_HELPERS + r"""
attach_disk WHOLE whole.raw
e2fsck -fn "${WHOLE}p3" > fsck5.out 2>&1 && echo "fsck5: clean" || { cat fsck5.out; echo "fsck5: DAMAGED"; }
# No partition named, and no GRUB entry: the disk of nos's own is found
disk_install status > disk-status.out 2>&1 || { cat disk-status.out; exit 1; }
cat disk-status.out
disk_install update --udpshell > disk-update.out 2>&1 || { cat disk-update.out; exit 1; }
cat disk-update.out
disk_install boot > disk-boot.out 2>&1 || { cat disk-boot.out; exit 1; }
cat disk-boot.out
if disk_install uninstall > disk-uninstall.out 2>&1; then
    echo "uninstall: done"
fi
cat disk-uninstall.out
if disk_install install "${WHOLE}p3" > disk-install.out 2>&1; then
    echo "install over it: done"
fi
cat disk-install.out
echo "grub.d: $(ls -A grub.d.disk | wc -l)"
mount_at "${WHOLE}p3" root ro
grub-editenv root/grubenv list > disk-grubenv2.out
same_as_image root disk-update
unmount_last
e2fsck -fn "${WHOLE}p3" > fsck6.out 2>&1 && echo "fsck6: clean" || { cat fsck6.out; echo "fsck6: DAMAGED"; }
"""


class Linux:
    """Runs a phase where losetup and mount work: here under sudo on Linux,
    in the builder image elsewhere."""

    def __init__(self, tmp):
        self.tmp = tmp
        self.native = platform.system() == "Linux"

    def run(self, name, script, partitions=False):
        """`partitions`: the phase's loop devices have partitions of their
        own (losetup -P), whose nodes the kernel makes in its /dev -- which
        a container sees only as its own /dev."""
        env = {"DISK_SIZE": str(DISK_SIZE), "HOST_START": str(HOST_START), "HOST_SIZE": str(HOST_SIZE),
               "NOS_START": str(NOS_START), "NOS_SIZE": str(NOS_SIZE), "WHOLE_SIZE": str(WHOLE_SIZE),
               "WHOLE_ROOT": WHOLE_ROOT}
        if self.native:
            env.update({"T": self.tmp, "IMAGE": os.path.join(self.tmp, "image.squashfs")})
            argv = ["sudo", "env"] + ["%s=%s" % kv for kv in env.items()] + ["bash", "-c", PRELUDE + script]
        else:
            env.update({"T": "/t", "IMAGE": "/t/image.squashfs"})
            argv = ["docker", "run", "--platform", "linux/amd64", "--rm", "--privileged", "-v", "%s:/t" % self.tmp]
            if partitions:
                argv += ["-v", "/dev:/dev"]
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

    def boot(self, name, disk="disk.raw", firmware=None):
        """`firmware`: UEFI's code and variables, for the two flash chips --
        SeaBIOS without it."""
        self.log = os.path.join(self.tmp, "serial-%s.log" % name)
        argv = ["qemu-system-x86_64", "-display", "none", "-no-reboot", "-m", "1G", "-smp", "2"]
        if firmware is not None:
            code, variables = firmware
            mine = os.path.join(self.tmp, "vars-%s.fd" % name)
            shutil.copyfile(variables, mine)
            argv += ["-drive", "if=pflash,format=raw,unit=0,readonly=on,file=%s" % code,
                     "-drive", "if=pflash,format=raw,unit=1,file=%s" % mine]
        argv += ["-drive", "file=%s,format=raw,if=none,id=d0" % os.path.join(self.tmp, disk),
                "-device", "virtio-blk-pci,drive=d0,bootindex=0,disable-legacy=on,disable-modern=off",
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


# A made-up machine for its microcode: the EX44's CPU, an i5-13500, as
# /proc/cpuinfo names it, and a file of one update for it, as Intel lays one
# out -- the original format, every checksum right -- of a revision of its own
FAKE_CPUINFO = ("processor\t: 0\nvendor_id\t: GenuineIntel\ncpu family\t: 6\nmodel\t\t: 191\n"
                "model name\t: 13th Gen Intel(R) Core(TM) i5-13500\nstepping\t: 2\n\nprocessor\t: 1\n")
FAKE_UCODE_NAME = "06-bf-02"
FAKE_UCODE_REVISION = 0x7777


def fake_ucode():
    def header(checksum):
        return struct.pack("<12I", 1, FAKE_UCODE_REVISION, 0x10212026, 0xB06F2, checksum, 1, 0x07, 0, 0, 0, 0, 0)
    data = bytes(2000)
    words = struct.unpack("<%dI" % ((48 + len(data)) // 4), header(0) + data)
    return header((-sum(words)) & 0xFFFFFFFF) + data


def expected_version(text):
    fields = dict(line.split("=", 1) for line in text.splitlines() if "=" in line)
    return "nos %s (%s)" % (fields.get("version", "?"), fields.get("revision", "?"))


# The management panel's lines nos-install --panel gives /etc/rc, in order
PANEL_RC = ["insmod /lib/modules/hv.ko", "hv on", "insmod /lib/modules/api.ko", "api start", "api panel create"]


def scrypt_matches(password, line):
    """Whether `line` -- scrypt$N$r$p$salt$hash, as the panel reads it -- is
    the hash of `password`."""
    parts = line.split("$")
    if len(parts) != 6 or parts[0] != "scrypt":
        return False
    n, r, p = (int(x) for x in parts[1:4])
    want = base64.b64decode(parts[5])
    got = hashlib.scrypt(password.encode(), salt=base64.b64decode(parts[4]), n=n, r=r, p=p,
                         maxmem=128 * n * r * 2, dklen=len(want))
    return got == want


def in_order(lines, wanted):
    at = 0
    for line in lines:
        if at < len(wanted) and line.strip() == wanted[at]:
            at += 1
    return at == len(wanted)


def ovmf():
    """UEFI for QEMU's two flash chips -- its code, and the variables a copy
    of which each boot takes -- as this host has it: QEMU's own (Homebrew's
    among them), or a distribution's ovmf package. None where there is none."""
    qemu = shutil.which("qemu-system-x86_64")
    share = os.path.join(os.path.dirname(os.path.dirname(os.path.realpath(qemu))), "share", "qemu")
    for code, variables in ((os.path.join(share, "edk2-x86_64-code.fd"), os.path.join(share, "edk2-i386-vars.fd")),
                            ("/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_VARS_4M.fd"),
                            ("/usr/share/OVMF/OVMF_CODE.fd", "/usr/share/OVMF/OVMF_VARS.fd"),
                            ("/usr/share/qemu/edk2-x86_64-code.fd", "/usr/share/qemu/edk2-i386-vars.fd"),
                            ("/usr/share/edk2/ovmf/OVMF_CODE.fd", "/usr/share/edk2/ovmf/OVMF_VARS.fd")):
        if os.path.exists(code) and os.path.exists(variables):
            return code, variables
    return None


def gpt_layout(dump):
    """sfdisk --dump's partitions, in order: each a dict of its fields, the
    numbers as numbers and the names unquoted."""
    parts = []
    for line in dump.splitlines():
        if " : " not in line:
            continue
        fields = {}
        for item in line.split(" : ", 1)[1].split(","):
            if "=" in item:
                key, value = (s.strip() for s in item.split("=", 1))
                fields[key] = int(value) if value.isdigit() else value.strip('"')
        parts.append(fields)
    return parts


def layout_ok(parts, root_mib, disk_bytes):
    """Whether the table is the one disk makes: BIOS boot at 1 MiB, the EFI
    System partition, the root of `root_mib`, and nos-data from right after
    it to the disk's end but for its last MiB -- each starting where the one
    before it ends, every type and name as it should be."""
    per_mib = MiB // SECTOR
    want = [(GPT_BIOS_BOOT, "BIOS boot", per_mib), (GPT_ESP, "EFI System", 512 * per_mib),
            (GPT_LINUX, "nos", root_mib * per_mib), (GPT_LINUX, "nos-data", None)]
    if len(parts) != len(want):
        return False
    at = per_mib
    for part, (kind, name, size) in zip(parts, want):
        if part.get("type", "").upper() != kind or part.get("name") != name or part.get("start") != at:
            return False
        if size is not None and part.get("size") != size:
            return False
        at = part["start"] + part["size"]
    # The last MiB holds the table's copy; the partition takes the rest
    return disk_bytes // SECTOR - per_mib - per_mib < at <= disk_bytes // SECTOR - per_mib + per_mib


def run_disk(tmp, linux, key, want):
    """A whole disk made nos's own (nos-install disk), booted under SeaBIOS
    and OVMF, then updated with no GRUB entry anywhere."""
    ok, out = linux.run("nos-install disk", WHOLE_PREPARE, partitions=True)
    if not check("nos-install disk", ok):
        return
    check("a disk that holds something is refused without --wipe, by what it holds, before anything is written",
          "holds: installed" not in out and "holds a dos partition table" in out and "--wipe erases it" in out and
          "holds: the disk is as it was" in out, linux.read("refuse-holds.out"))
    check("a partition is refused: disk takes a whole disk",
          "part: installed" not in out and "is a partition" in linux.read("refuse-part.out"),
          linux.read("refuse-part.out"))
    check("a disk with a filesystem of it mounted is refused, --wipe or not",
          "mounted: installed" not in out and "is in use: mounted at" in linux.read("refuse-mounted.out"),
          linux.read("refuse-mounted.out"))
    check("the root is 10G unless told, and a disk with no room for it is refused before anything is written",
          "size: installed" not in out and "no room for a root of 10240 MiB" in linux.read("refuse-size.out") and
          "size: the disk is as it was" in out, linux.read("refuse-size.out"))
    layout = linux.read("layout.out")
    check("its table: GPT -- BIOS boot from 1 MiB, EFI System, the root of the size asked for, nos-data to the end",
          "label: gpt" in layout and layout_ok(gpt_layout(layout), 1024, WHOLE_SIZE), layout)
    check("GRUB's boot code is in the disk's first sector, its core image and modules for BIOS on the root",
          "mbr: boot code" in out and "root: GRUB for BIOS" in out, out)
    check("GRUB for UEFI is at \\EFI\\BOOT\\BOOTX64.EFI on the EFI System partition, its modules on the root",
          "esp: BOOTX64.EFI" in out and "root: GRUB for UEFI" in out, out)
    check("fsck.fat finds the EFI System partition clean, e2fsck the root", "esp: clean" in out and "root: clean" in out,
          linux.read("fsckfat.out") + linux.read("fsckroot.out"))
    check("nos-data has nothing on it", "data: blank" in out, out)
    root_uuid = linux.read("root-uuid.out").strip()
    data_guid = linux.read("data-partuuid.out").strip()
    check("the root's grub.cfg is the disk's own, and finds the root by its UUID",
          linux.read("grubcfg-first.out").strip() == STANDALONE_MARK and
          linux.read("grubcfg-uuid.out").strip() == root_uuid and len(root_uuid) == 36,
          linux.read("grubcfg-first.out") + linux.read("grubcfg-uuid.out") + root_uuid)
    check("every file of the image is on the root, the key in authorized_keys, the parameters in the grubenv",
          "disk: every file of the image" in out and "keys: 1" in out and
          "nos_cmdline=netconsole=10.0.2.2:6666" in linux.read("disk-grubenv.out"), out)
    check("nothing of the machine's GRUB is touched", "grub.d: 0" in out, out)

    m = Machine(tmp, key)
    for name, firmware in (("BIOS", None), ("UEFI", ovmf())):
        if name == "UEFI" and firmware is None:
            check("UEFI for QEMU here: QEMU's edk2-x86_64-code.fd, or the ovmf package", False)
            break
        m.boot("disk-" + name.lower(), disk="whole.raw", firmware=firmware)
        try:
            check("the disk boots nos alone, under %s" % name, m.wait_log("boot: complete", BOOT_TIMEOUT),
                  m.text()[-3000:])
            check("its root is the disk's, read-write (%s)" % name,
                  "MountRootFs: mounted ext4 on /" in m.text() and "(rw)" in m.text(), m.text()[-3000:])
            version = m.wait_ssh(300)
            check("sshd lets in the key disk was given, to the image's kernel (%s)" % name, version == want,
                  "%r, the image's VERSION says %r" % (version, want))
            if version is not None:
                rc, out = m.ssh(["cat", "/proc/cmdline"])
                check("the command line names the root by its UUID, with the parameters the grubenv keeps (%s)" % name,
                      rc == 0 and ("root=UUID=" + root_uuid) in out and "netconsole=10.0.2.2:6666" in out, out)
                rc, out = m.ssh(["disks"])
                check("nos sees nos-data by its GUID (%s)" % name,
                      rc == 0 and len(data_guid) == 36 and ("part:" + data_guid) in out, out)
            m.ssh(["poweroff"], timeout=60)
            check("nos powers off (%s)" % name, m.wait_exit(120))
        finally:
            m.stop()

    ok, out = linux.run("status and update of a disk of nos's own", WHOLE_AFTER, partitions=True)
    if not check("status and update of a disk of nos's own", ok):
        return
    check("e2fsck finds the root clean after nos ran on it", "fsck5: clean" in out, out)
    status = linux.read("disk-status.out")
    check("status finds the disk of nos's own by itself, and says its own GRUB boots nos by default",
          "the root of a disk of nos's own" in status and "booted by:   its disk's own GRUB" in status and
          "next boot:   nos, its disk's default" in status, status)
    update = linux.read("disk-update.out")
    check("so does update, and it leaves every GRUB of the machine alone",
          "its disk's own GRUB boots it" in update and "grub.d: 0" in out and "update-grub" not in update, update)
    check("update puts every file of the image there again, and takes the UDP shell it was given",
          "disk-update: every file of the image" in out and
          "nos_cmdline=udpshell=9000" in linux.read("disk-grubenv2.out").splitlines(), linux.read("disk-grubenv2.out"))
    check("boot has nothing to arm there", "nothing to arm" in linux.read("disk-boot.out"), linux.read("disk-boot.out"))
    check("uninstall has nothing to take out", "uninstall: done" not in out, linux.read("disk-uninstall.out"))
    check("an install over its root is refused: update is what renews it",
          "install over it: done" not in out and "nos-install update" in linux.read("disk-install.out"),
          linux.read("disk-install.out"))
    check("e2fsck finds the root clean after the update", "fsck6: clean" in out, out)


def run(tmp, image):
    linux = Linux(tmp)
    shutil.copy(image, os.path.join(tmp, "image.squashfs"))
    key = os.path.join(tmp, "id_test")
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "image-test", "-f", key], check=True)

    with open(os.path.join(tmp, "cpuinfo"), "w") as f:
        f.write(FAKE_CPUINFO)
    os.makedirs(os.path.join(tmp, "intel-ucode"))
    with open(os.path.join(tmp, "intel-ucode", FAKE_UCODE_NAME), "wb") as f:
        f.write(fake_ucode())

    ok, out = linux.run("nos-install install", PREPARE)
    if not check("nos-install install", ok):
        return linux
    check("an image with a file nos-install does not know is refused, by its name, before anything is written",
          "stray: installed" not in out and "the image has /lib/stray, which" in out and
          "stray: the partition is as it was" in out, [line for line in out.splitlines() if "stray" in line])
    check("install puts every file of the image on the partition, as it is in the image",
          "install: every file of the image" in out, [line for line in out.splitlines() if line.startswith("install: ")])
    check("install puts the machine's microcode beside the kernel",
          "intel-ucode/%s, revision 0x%x" % (FAKE_UCODE_NAME, FAKE_UCODE_REVISION) in out, out)
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
        check("GRUB hands the kernel the microcode as the module ucode, and the kernel says it loads none under "
              "QEMU", "Module ucode 0x" in text and "microcode: not loaded:" in text,
              [line for line in text.splitlines() if "ucode" in line or "microcode" in line][:6])
        check("its root is the partition, by UUID, read-write",
              "MountRootFs: mounted ext4 on /" in text and "(rw)" in text, text[-3000:])
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
    check("so does update", "update: every file of the image" in out,
          [line for line in out.splitlines() if line.startswith("update: ")])
    layout = linux.read("layout2.out").split()
    check("update keeps the old kernel, modules, hvboot, licenses and VERSION for a rollback",
          all(p in layout for p in ("kernel64.elf.prev", "modules.prev", "hvboot.prev", "licenses.prev",
                                    "VERSION.prev")), layout)
    notice = linux.read("img-license.out")
    check("the image carries the notice of the CRYPTOGAMS assembly its kernel and modules run, as its "
          "license asks of a binary", "CRYPTOGAMS by <appro@openssl.org>" in notice and
          "Redistributions in binary form" in notice, notice[:200])
    env2 = linux.read("grubenv2.out")
    check("update takes the UDP shell it was given, and arms a boot",
          "nos_cmdline=udpshell=9000" in env2 and "nos_next=nos" in env2, env2)
    said = [line.split("its password ", 1)[1].strip() for line in out.splitlines()
            if "the panel's user is root, its password " in line]
    password = said[0] if len(said) == 1 else None
    check("update --panel prints root's password for the panel, once", password is not None and
          out.count(password) == 1, [line for line in out.splitlines() if "panel" in line])
    hashed = linux.read("panelpass2.out").strip()
    check("and keeps its scrypt hash alone on the partition, as the panel checks it",
          password is not None and scrypt_matches(password, hashed) and password not in hashed, hashed)
    rc2 = [line.strip() for line in linux.read("rc2.out").splitlines()]
    check("and gives /etc/rc the panel's lines, in order, after the image's own",
          in_order(rc2, PANEL_RC) and "insmod /lib/modules/sshd.ko" in rc2 and
          rc2.index("insmod /lib/modules/sshd.ko") < rc2.index("api panel create"), rc2)
    has_bundle = linux.read("img-bundle.out").startswith("url=")

    # Boot 3: the update, with the UDP shell asked for
    m.boot("3")
    try:
        check("nos boots again", m.wait_log("boot: complete", BOOT_TIMEOUT), m.text()[-3000:])
        version = m.wait_ssh(300)
        check("sshd lets the key in again, to the same kernel", version == want, "%r != %r" % (version, want))
        check("with the host key it made the first time", m.host_key() == first_key)
        out = m.udp_shell("version", timeout=10)
        check("the UDP shell answers, now that it was asked for", out is not None and out.startswith("nos "), out)
        # /etc/rc starts sshd in its first lines and the API after hv's: a
        # session may come in before `api start` has run.
        for _ in range(API_WAIT):
            rc, out = m.ssh(["api"])
            if rc == 0 and "api: serving port 39234" in out:
                break
            time.sleep(1)
        check("/etc/rc's api.ko serves the host API", rc == 0 and "api: serving port 39234" in out, out)
        rc, out = m.ssh(["dmesg"])
        if has_bundle:
            print("  (the image has /lib/panel/bundle: its api panel create downloads, which this test does not)")
        else:
            check("its api panel create makes no panel of an image released without one, and says why",
                  rc == 0 and "no /lib/panel/bundle" in out,
                  [line for line in out.splitlines() if "api" in line][-8:])
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

    run_disk(tmp, linux, key, want)
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
