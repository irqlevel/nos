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
written: the kernel, its modules, hvboot and CloudHv replaced -- every file
of the image there again -- and the old ones kept for a rollback, with their
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
refused. A further boot gives nos two NICs: DHCP on eth0 inside, a static
address, the default route and sshd on eth1 outside, and an additional
private route through eth0.

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
# is in the image -- nos-install and nos-setup aside, which stay in it, and
# the FILEs named, which are the machine's by then -- or the ones that are
# not are named. Not from nos-install's list of what goes where: a file it
# leaves out is one that list does not have
same_as_image() {
    local f bad=0 dir=$1 when=$2
    shift 2
    while IFS= read -r f; do
        case " $* " in *" $f "*) continue ;; esac
        cmp -s "img/$f" "$dir/$f" || { echo "$when: not on the partition as in the image: /$f"; bad=1; }
    done < <(cd img && find . -type f ! -path ./nos-install ! -path ./nos-setup | sed 's|^\./||' | sort)
    [ "$bad" = 1 ] || echo "$when: every file of the image"
}
mount_at "$IMAGE" img loop,ro
cp img/VERSION img-version.out
cat img/lib/panel/bundle > img-bundle.out 2>/dev/null || echo none > img-bundle.out
cat img/lib/licenses/CRYPTOGAMS > img-license.out 2>/dev/null || echo none > img-license.out
cat img/lib/licenses/CLOUDHV > img-cloudhv-license.out 2>/dev/null || echo none > img-cloudhv-license.out
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
cat part/etc/net.toml > nettoml1.out 2>/dev/null || echo none > nettoml1.out
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

# An install from before /etc/net.toml, as a server with a static address
# has one: the network the kernel parameters' words, and a kernel that does
# not read the file -- which the update keeps for a rollback
mount_at "$NOS" part
rm -f part/etc/net.toml
grub-editenv part/grubenv set "nos_cmdline=ip=10.0.2.15/24 gw=10.0.2.2 dns=10.0.2.3"
# (its bytes not the path a kernel that reads the file has in its image)
printf 'a kernel from before the settings file\n' > part/boot/kernel64.elf
unmount_last

# no partition named: the GRUB entry names it
nos_install update --udpshell --boot-once --panel > update.out 2>&1 || { cat update.out; exit 1; }
cat update.out

mount_at "$NOS" part ro
cp part/etc/rc rc2.out
cat part/etc/api/panel.pass > panelpass2.out 2>/dev/null || echo none > panelpass2.out
grub-editenv part/grubenv list > grubenv2.out
cat part/etc/net.toml > nettoml2.out 2>/dev/null || echo none > nettoml2.out
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
# Again: the kernel kept for a rollback is the image's now, which reads
# /etc/net.toml, and the words a kernel before it needed go
nos_install update > update3.out 2>&1 || { cat update3.out; exit 1; }
cat update3.out
mount_at "$NOS" part ro
grub-editenv part/grubenv list > grubenv3.out
unmount_last
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
# nos-install on a disk of nos's own -- under BIOS unless told: this machine's
# own firmware is never the test's to set (a CI runner boots by UEFI, and its
# efibootmgr is real); under UEFI the stand-in's (efi_install)
disk_install() {
    mkdir -p "$T/grub.d.disk"
    NOS_FIRMWARE=${NOS_FIRMWARE:-bios} \
        NOS_GRUB_D="$T/grub.d.disk" NOS_UPDATE_GRUB=false NOS_ALLOW_ANY_DISK=1 \
        NOS_CPUINFO="$T/cpuinfo" NOS_UCODE_DIR="$T/intel-ucode" "$T/img/nos-install" "$@"
}
first_mib() { dd if="$1" bs=1M count=1 status=none | sha256sum | cut -d' ' -f1; }
blank_mib() { [ -z "$(dd if="$1" bs=1M count=1 status=none | tr -d '\000' | head -c 1)" ]; }
# efibootmgr's stand-in, over a firmware's entries kept in $EFI_STATE: lists
# them as efibootmgr does (-v with the device paths), and takes -o, -n, -a
# and -C as efibootmgr does, each call's arguments kept in $EFI_STATE.log.
# As efibootmgr 18 does -- Ubuntu 24.04's, Debian 12's: the listing an entry
# at a time, read from efivarfs in between, so a reader that leaves at what
# it wanted (grep -q) leaves it writing into a closed pipe, which pipefail
# takes for the answer -- what made nos-install warn on the Supermicro
# X11SSL-F that it could not turn on an entry that was on; a new entry the
# lowest number no entry has, on, listed in its place; and -a finding only
# the last entry it lists, every other one "not found" (rhboot/efibootmgr#186)
cat > "$T/efibootmgr" <<'FAKE'
#!/bin/bash
state=$EFI_STATE
echo "$*" >> "$state.log"
verbose=0 order= next= create=0 disk= part= label= loader= num= activate=0
while [ $# -gt 0 ]; do
    case "$1" in
        -v) verbose=1 ;;
        -q) ;;
        -o) order=$2; shift ;;
        -n) next=$2; shift ;;
        -C) create=1 ;;
        -d) disk=$2; shift ;;
        -p) part=$2; shift ;;
        -L) label=$2; shift ;;
        -l) loader=$2; shift ;;
        -b) num=$2; shift ;;
        -a) activate=1 ;;
        *) echo "efibootmgr stand-in: what is $1?" >&2; exit 2 ;;
    esac
    shift
done
[ -z "$order" ] || sed -i "s/^BootOrder: .*/BootOrder: $order/" "$state"
if [ -n "$next" ]; then
    grep -v '^BootNext:' "$state" > "$state.new"
    echo "BootNext: $next" >> "$state.new"
    mv "$state.new" "$state"
fi
if [ "$activate" = 1 ]; then
    if [ "$(grep -o '^Boot[0-9A-F]\{4\}' "$state" | tail -n 1)" != "Boot$num" ]; then
        printf 'efibootmgr: Boot entry %x not found\n' $((16#$num)) >&2
        exit 16
    fi
    sed -i "s/^Boot$num  /Boot$num* /" "$state"
fi
if [ "$create" = 1 ]; then
    guid=$(blkid -p -o value -s PART_ENTRY_UUID "${disk}p$part")
    n=0
    while grep -q "^Boot$(printf %04X $n)" "$state"; do
        n=$((n + 1))
    done
    NUM=$(printf %04X $n) \
    LINE=$(printf 'Boot%04X* %s\tHD(%s,GPT,%s,0x800,0x100000)/File(%s)' $n "$label" "$part" "$guid" "$loader") \
        awk '!put && /^Boot[0-9A-F][0-9A-F][0-9A-F][0-9A-F]/ && substr($0, 5, 4) > ENVIRON["NUM"] {
                 print ENVIRON["LINE"]
                 put = 1
             }
             { print }
             END { if (!put) print ENVIRON["LINE"] }' "$state" > "$state.new"
    mv "$state.new" "$state"
fi
[ -z "$order$next$num" ] && [ "$create" = 0 ] || exit 0
slowly() {
    while IFS= read -r line; do
        printf '%s\n' "$line"
        sleep 0.02
    done
}
if [ "$verbose" = 1 ]; then
    slowly < "$state"
else
    sed 's/\t.*//' "$state" | slowly
fi
FAKE
chmod 755 "$T/efibootmgr"
# A firmware's entries: the Supermicro X11SSD-F's, as its efibootmgr -v had
# them when it booted its old Ubuntu after an install of nos -- PXE on its two
# ports first, the Ubuntu entry, HTTP boot, AMI's own "UEFI OS" entries, the
# shell, a legacy one turned off; with $1 the GPT GUID of a "UEFI OS" entry's
# EFI System partition, and none for nos's when $1 is empty; with $2 too, an
# entry numbered after that one -- the firmware's IPv6 PXE, out of the order
# -- which leaves it not the last efibootmgr lists
efi_state() {
    cat <<EOF
BootCurrent: 0000
Timeout: 1 seconds
BootOrder: 0067,0069,0000,0066,0068,006A${1:+,006B},0064,0063
Boot0000* Ubuntu	HD(2,GPT,12fe60cd-809b-4681-ac0f-67a3e80e0f7f,0x1000,0x2f800)/File(\EFI\UBUNTU\GRUBX64.EFI)
      dp: 04 01 2a 00 02 00 00 00 00 10 00 00 00 00 00 00 00 f8 02 00 00 00 00 00
Boot0063  Hard Drive	BBS(HD,,0x0)0000474f00004e4fa9000000000000006f0049004e00540045004c00
Boot0064* UEFI: Built-in EFI Shell	VenMedia(5023b95c-db26-429b-a648-bd47664c8012)0000424f
Boot0066* UEFI: Intel(R) I350 Gigabit Network Connection	PciRoot(0x0)/Pci(0x1,0x1)/Pci(0x0,0x0)/MAC(ac1f6b205352,1)/IPv4(0.0.0.00.0.0.0,0,0)/Uri()0000424f
Boot0067* UEFI: IP4 Intel(R) I350 Gigabit Network Connection	PciRoot(0x0)/Pci(0x1,0x1)/Pci(0x0,0x0)/MAC(ac1f6b205352,1)/IPv4(0.0.0.00.0.0.0,0,0)0000424f
Boot0068* UEFI: Intel(R) I350 Gigabit Network Connection	PciRoot(0x0)/Pci(0x1,0x1)/Pci(0x0,0x1)/MAC(ac1f6b205353,1)/IPv4(0.0.0.00.0.0.0,0,0)/Uri()0000424f
Boot0069* UEFI: IP4 Intel(R) I350 Gigabit Network Connection	PciRoot(0x0)/Pci(0x1,0x1)/Pci(0x0,0x1)/MAC(ac1f6b205353,1)/IPv4(0.0.0.00.0.0.0,0,0)0000424f
Boot006A* UEFI OS	HD(2,GPT,12fe60cd-809b-4681-ac0f-67a3e80e0f7f,0x1000,0x2f800)/File(\EFI\BOOT\BOOTX64.EFI)0000424f
EOF
    if [ -n "${1:-}" ]; then
        printf 'Boot006B  UEFI OS\tHD(2,GPT,%s,0x1000,0x100000)/File(\\EFI\\BOOT\\BOOTX64.EFI)0000424f\n' "$1"
    fi
    if [ -n "${2:-}" ]; then
        printf 'Boot006C* UEFI: PXE IPv6 Intel(R) I350 Gigabit Network Connection\tPciRoot(0x0)/Pci(0x1,0x1)/Pci(0x0,0x0)/MAC(ac1f6b205352,1)/IPv6([::]:<->[::]:,0,0)0000424f\n'
    fi
}
# nos-install under UEFI, the firmware the stand-in's
efi_install() {
    NOS_FIRMWARE=uefi NOS_IN_VM=${NOS_IN_VM:-0} NOS_EFIBOOTMGR="$T/efibootmgr" disk_install "$@"
}
# The one-line installer as a rescue system runs it (curl ... | sh -s --
# ARGS): install.sh fetches the image and its SHA256SUMS from a release -- a
# directory standing in for one, by file:// -- checks the one against the
# other, mounts it and runs its nos-install; it installs no packages here
script_install() {
    mkdir -p "$T/grub.d.disk"
    NOS_RELEASE_URL=${NOS_RELEASE_URL:-file://$T/release} NOS_NO_PACKAGES=1 \
        NOS_FIRMWARE=${NOS_FIRMWARE:-bios} \
        NOS_GRUB_D="$T/grub.d.disk" NOS_UPDATE_GRUB=false NOS_ALLOW_ANY_DISK=1 \
        NOS_CPUINFO="$T/cpuinfo" NOS_UCODE_DIR="$T/intel-ucode" sh "$T/install.sh" "$@"
}
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

# The one-line installer refuses an image that is not the release's -- the
# release's SHA256SUMS another's -- before anything is written
before=$(first_mib "$WHOLE")
if NOS_RELEASE_URL="file://$T/release-bad" script_install disk "$WHOLE" --wipe --root-size "$WHOLE_ROOT" \
    > refuse-sum.out 2>&1; then
    echo "sum: installed"
fi
cat refuse-sum.out
[ "$(first_mib "$WHOLE")" != "$before" ] || echo "sum: the disk is as it was"
# ... and with no arguments, its dialog, with no terminal to draw it on
if setsid -w sh -c 'NOS_RELEASE_URL="file://$T/release" NOS_NO_PACKAGES=1 sh "$T/install.sh"' \
    < /dev/null > script-noterm.out 2>&1; then
    echo "noterm: ran"
fi
cat script-noterm.out
# The image's dialog, from the image on a Linux not the installer ISO's:
# with no terminal, it says so and does nothing
if "$T/img/nos-setup" < /dev/null > setup-noterm.out 2>&1; then
    echo "setup noterm: ran"
fi
cat setup-noterm.out
sha256sum "$T/img/nos-setup" | cut -d' ' -f1 > img-setup.sum

# Under UEFI, on the Supermicro's firmware as it was before nos: no entry
# for the disk, which nos-install makes, and puts right after PXE's -- the
# install made by the one-line installer, from the release
efi_state > efi-before
EFI_STATE="$T/efi-before" NOS_FIRMWARE=uefi NOS_IN_VM=0 NOS_EFIBOOTMGR="$T/efibootmgr" \
    script_install disk "$WHOLE" --wipe --root-size "$WHOLE_ROOT" --ssh-key id_test.pub \
    --netconsole 10.0.2.2:6666 > disk.out 2>&1 || { cat disk.out; exit 1; }
cat disk.out
ls -d /tmp/nos-install.* 2>/dev/null && echo "script: left in /tmp"
grep -q ' /tmp/nos-install\.' /proc/mounts && echo "script: left mounted"
cat efi-before efi-before.log
sfdisk --dump "$WHOLE" > layout.out
cat layout.out
fsck.vfat -n "${WHOLE}p2" > fsckfat.out 2>&1 && echo "esp: clean" || { cat fsckfat.out; echo "esp: DAMAGED"; }
e2fsck -fn "${WHOLE}p3" > fsckroot.out 2>&1 && echo "root: clean" || { cat fsckroot.out; echo "root: DAMAGED"; }
[ -n "$(blkid -p -o value -s TYPE "${WHOLE}p4" || true)" ] || ! blank_mib "${WHOLE}p4" || echo "data: blank"
[ -z "$(dd if="$WHOLE" bs=440 count=1 status=none | tr -d '\000' | head -c 1)" ] || echo "mbr: boot code"
blkid -p -o value -s PART_ENTRY_UUID "${WHOLE}p4" > data-partuuid.out
blkid -p -o value -s UUID "${WHOLE}p3" > root-uuid.out
blkid -p -o value -s PART_ENTRY_UUID "${WHOLE}p2" > esp-guid.out
mount_at "${WHOLE}p2" esp ro
[ ! -f esp/EFI/BOOT/BOOTX64.EFI ] || echo "esp: BOOTX64.EFI"
unmount_last
mount_at "${WHOLE}p3" root ro
head -n 1 root/boot/grub/grub.cfg > grubcfg-first.out
sed -n 's/^set nos_uuid=//p' root/boot/grub/grub.cfg > grubcfg-uuid.out
[ ! -f root/boot/grub/i386-pc/core.img ] || echo "root: GRUB for BIOS"
[ ! -d root/boot/grub/x86_64-efi ] || echo "root: GRUB for UEFI"
grub-editenv root/grubenv list > disk-grubenv.out
cat root/etc/net.toml > disk-nettoml.out 2>/dev/null || echo none > disk-nettoml.out
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
debugfs -R 'cat /grubenv' "${WHOLE}p3" 2>/dev/null | grep '^nos_' > disk-grubenv2.out
disk_install boot > disk-boot.out 2>&1 || { cat disk-boot.out; exit 1; }
cat disk-boot.out
# The Supermicro's firmware as an install of nos left it: AMI's own entry
# for the disk's \EFI\BOOT\BOOTX64.EFI, turned off, after Ubuntu's
esp_guid=$(blkid -p -o value -s PART_ENTRY_UUID "${WHOLE}p2")
efi_state "$esp_guid" > efi-once
EFI_STATE="$T/efi-once" NOS_IN_VM=1 efi_install boot > efi-once.out 2>&1 || { cat efi-once.out; exit 1; }
cat efi-once.out efi-once.log
grep -E '^(BootOrder|BootNext):' efi-once
efi_state "$esp_guid" > efi-default
EFI_STATE="$T/efi-default" efi_install boot --default > efi-default.out 2>&1 || { cat efi-default.out; exit 1; }
cat efi-default.out efi-default.log
grep -E '^(BootOrder|BootNext|Boot006B)' efi-default
# ... with AMI's entry off and not the last efibootmgr lists, which
# efibootmgr 18 cannot turn on: an entry of nos's own made beside it
efi_state "$esp_guid" after > efi-off
EFI_STATE="$T/efi-off" efi_install boot > efi-off.out 2>&1 || { cat efi-off.out; exit 1; }
cat efi-off.out efi-off.log
grep -E '^(BootOrder|BootNext|Boot0001|Boot006B)' efi-off
# The network as this Linux has it, for nos: a rescue system's is the server's
command -v ip > /dev/null ||
    { echo "no ip here (iproute2): a builder image older than the Dockerfile -- docker build -t nos-builder ."; exit 1; }
here_dev=$(ip -4 route show default | sed -n 's/.* dev \([^ ]*\).*/\1/p' | head -n 1)
echo "here: ip=$(ip -4 -o addr show dev "$here_dev" scope global | awk '{ print $4; exit }') gw=$(ip -4 route show default | sed -n 's/.* via \([0-9.]*\).*/\1/p' | head -n 1)"
disk_install network > disk-network.out 2>&1 || { cat disk-network.out; exit 1; }
cat disk-network.out
disk_install update --network-here > disk-here.out 2>&1 || { cat disk-here.out; exit 1; }
cat disk-here.out
debugfs -R 'cat /grubenv' "${WHOLE}p3" 2>/dev/null | grep '^nos_' > disk-grubenv3.out
debugfs -R 'cat /etc/net.toml' "${WHOLE}p3" 2>/dev/null > disk-nettoml3.out
# What the kernel would refuse is refused before anything is written, and
# so are the network's words on the kernel command line
for bad in '--cmdline ip=10.0.2.15/24' '--ip 10.0.2.0/24' '--ip 10.0.2.15/24 --gw 10.0.3.1' '--gw 10.0.2.2' \
           '--ip 10.0.2.15/24 --dhcp' '--ip 127.0.0.1/8' '--dns 224.0.0.1' '--network-here --ip 10.0.2.15/24'; do
    if disk_install update $bad > disk-bad.out 2>&1; then
        echo "network refused: no, $bad"
    else
        echo "network refused: $bad: $(tail -n 1 disk-bad.out)"
    fi
done
debugfs -R 'cat /etc/net.toml' "${WHOLE}p3" 2>/dev/null > disk-nettoml4.out
# Two NICs under QEMU next: DHCP inside, a static address and sshd outside.
mount_at "${WHOLE}p3" root
cat > root/etc/net.toml <<'EOF'
dhcp_eth0 = true
ip_eth1 = "198.51.100.5/24"
gw_eth1 = "198.51.100.2"
dns = "198.51.100.3"
default = "eth1"
route_eth0 = ["10.0.0.0/8 via 10.0.2.2"]
EOF
sed -i -E 's/^sshd start( 22 nic=eth[0-9]+)?$/sshd start 22 nic=eth1/' root/etc/rc
unmount_last
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
same_as_image root disk-update etc/rc
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

    def boot(self, name, disk="disk.raw", firmware=None, two_nics=False):
        """`firmware`: UEFI's code and variables, for the two flash chips --
        SeaBIOS without it. `two_nics`: DHCP inside, a static network outside."""
        self.log = os.path.join(self.tmp, "serial-%s.log" % name)
        argv = ["qemu-system-x86_64", "-display", "none", "-no-reboot", "-m", "1G", "-smp", "2"]
        if firmware is not None:
            code, variables = firmware
            mine = os.path.join(self.tmp, "vars-%s.fd" % name)
            shutil.copyfile(variables, mine)
            argv += ["-drive", "if=pflash,format=raw,unit=0,readonly=on,file=%s" % code,
                     "-drive", "if=pflash,format=raw,unit=1,file=%s" % mine]
        argv += ["-drive", "file=%s,format=raw,if=none,id=d0" % os.path.join(self.tmp, disk),
                "-device", "virtio-blk-pci,drive=d0,bootindex=0,disable-legacy=on,disable-modern=off"]
        if two_nics:
            argv += ["-netdev", "user,id=n0,net=10.0.2.0/24,host=10.0.2.2,dns=10.0.2.3,dhcpstart=10.0.2.15",
                     "-device", "virtio-net-pci,netdev=n0,disable-legacy=on,disable-modern=off",
                     "-netdev", "user,id=n1,net=198.51.100.0/24,host=198.51.100.2,dns=198.51.100.3,"
                     "dhcpstart=198.51.100.5,hostfwd=tcp:127.0.0.1:%d-:22,hostfwd=udp:127.0.0.1:%d-:%d"
                     % (self.ssh_port, self.udp_port, SHELL_PORT),
                     "-device", "virtio-net-pci,netdev=n1,disable-legacy=on,disable-modern=off"]
        else:
            argv += ["-netdev", "user,id=n0,hostfwd=tcp:127.0.0.1:%d-:22,hostfwd=udp:127.0.0.1:%d-:%d"
                     % (self.ssh_port, self.udp_port, SHELL_PORT),
                     "-device", "virtio-net-pci,netdev=n0,disable-legacy=on,disable-modern=off"]
        argv += ["-device", "virtio-rng-pci", "-serial", "file:" + self.log]
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
    check("the one-line installer refuses an image its release's SHA256SUMS does not have, before anything is "
          "written", "sum: installed" not in out and "is not the release's" in linux.read("refuse-sum.out") and
          "sum: the disk is as it was" in out, linux.read("refuse-sum.out"))
    check("with no arguments and no terminal, it says the dialog needs one, and fetches nothing",
          "noterm: ran" not in out and "the dialog needs a terminal" in linux.read("script-noterm.out") and
          "fetching" not in linux.read("script-noterm.out"), linux.read("script-noterm.out"))
    check("the image carries the dialog, nos-setup as the tree has it",
          linux.read("img-setup.sum").strip() == digest_of(os.path.join(ROOT, "scripts", "installer", "nos-setup")),
          linux.read("img-setup.sum"))
    check("run from the image on a Linux not the ISO's with no terminal, it says so and does nothing",
          "setup noterm: ran" not in out and "the dialog needs a terminal" in linux.read("setup-noterm.out"),
          linux.read("setup-noterm.out"))
    disk_out = linux.read("disk.out")
    check("the one-line installer fetches the image and its sums from the release, checks them, mounts it and "
          "runs its nos-install -- the install below is its",
          "nos: fetching file://" in disk_out and (", sha256 %s" % digest_of(os.path.join(tmp, "image.squashfs")))
          in disk_out and "a GPT of nos's own" in disk_out, disk_out)
    check("and leaves nothing behind: no image in /tmp, nothing mounted",
          "script: left in /tmp" not in out and "script: left mounted" not in out, out)
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
    disk_nettoml = linux.read("disk-nettoml.out")
    check("disk writes /etc/net.toml: DHCP, given no network", nettoml_values(disk_nettoml) == {"dhcp": "true"},
          disk_nettoml)
    efi = linux.read("efi-before")
    efi_log = linux.read("efi-before.log")
    esp_guid = linux.read("esp-guid.out").strip()
    check("under UEFI, with no entry of the firmware's for the disk, one is made for its \\EFI\\BOOT\\BOOTX64.EFI "
          "-- the lowest number free, as efibootmgr takes it",
          ("Boot0001* nos\tHD(2,GPT,%s," % esp_guid) in efi and "File(\\EFI\\BOOT\\BOOTX64.EFI)" in efi,
          efi + efi_log)
    check("and put right after the leading PXE entries, nothing else moved: the Supermicro's old Ubuntu next",
          "BootOrder: 0067,0069,0001,0000,0066,0068,006A,0064,0063" in efi, efi)
    check("the entry it made is on, and is not turned on again, nor said to be off -- efibootmgr's listing read "
          "whole, an entry at a time as efibootmgr 18 writes it",
          all("-a" not in line.split() for line in efi_log.splitlines()) and "could not turn" not in disk_out,
          efi_log + disk_out)

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
                rc, out = m.ssh(["net"])
                check("eth0's settings are /etc/net.toml's: DHCP (%s)" % name,
                      rc == 0 and "eth0 settings: DHCP, the lease's resolver (/etc/net.toml)" in out, out[-400:])
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
    check("boot under BIOS says the BIOS's own order picks the disk", "boots by BIOS" in linux.read("disk-boot.out"),
          linux.read("disk-boot.out"))
    once = linux.read("efi-once")
    check("boot under UEFI, on the Supermicro's firmware as an install left it: BootNext is the firmware's own "
          "entry for the disk, turned on, and the order is as it was",
          "BootNext: 006B" in once and "Boot006B* UEFI OS" in once and
          "BootOrder: 0067,0069,0000,0066,0068,006A,006B,0064,0063" in once, once + linux.read("efi-once.log"))
    check("in a virtual machine, it says the server's own order is set from the server",
          "runs in a virtual machine" in linux.read("efi-once.out") and "nos-install boot --default" in
          linux.read("efi-once.out"), linux.read("efi-once.out"))
    default = linux.read("efi-default")
    check("boot --default puts that entry right after PXE's, for good: the order the Supermicro needed",
          "BootOrder: 0067,0069,006B,0000,0066,0068,006A,0064,0063" in default and "BootNext" not in default and
          "Boot006B* UEFI OS" in default, default + linux.read("efi-default.log"))
    off = linux.read("efi-off")
    off_out = linux.read("efi-off.out")
    check("boot under UEFI, the firmware's entry for the disk off and not the last efibootmgr lists -- which "
          "efibootmgr 18 cannot turn on: an entry of nos's own is made beside it, on, BootNext is that one, the order "
          "as it was, and nothing is warned",
          "BootNext: 0001" in off and ("Boot0001* nos\tHD(2,GPT,%s," % esp_guid) in off and
          "Boot006B  UEFI OS" in off and "BootOrder: 0067,0069,0000,0066,0068,006A,006B,0064,0063" in off and
          "WARNING" not in off_out, off + off_out + linux.read("efi-off.log"))
    here = [line for line in out.splitlines() if line.startswith("here: ")]
    nettoml3 = linux.read("disk-nettoml3.out")
    got = nettoml_values(nettoml3)
    said = dict(word.split("=", 1) for word in here[0][len("here: "):].split()) if len(here) == 1 else {}
    check("update --network-here writes the address, gateway and DNS server this Linux has to /etc/net.toml",
          said != {} and got.get("ip") == said.get("ip") and got.get("gw") == said.get("gw") and "dns" in got,
          here + [nettoml3])
    check("and leaves the kernel parameters as they were",
          "nos_cmdline=udpshell=9000" in linux.read("disk-grubenv3.out").splitlines(), linux.read("disk-grubenv3.out"))
    network = linux.read("disk-network.out")
    words = network.strip().splitlines()[-1].split() if network.strip() else []
    check("network says the same, as the dialog offers it",
          words == ["ip=%s" % got.get("ip"), "gw=%s" % got.get("gw"), "dns=%s" % got.get("dns")], network + nettoml3)
    refused = [line for line in out.splitlines() if line.startswith("network refused: ")]
    check("nos-install refuses what the kernel would refuse of a network, and ip= in --cmdline, before writing",
          len(refused) == 8 and not any(line.startswith("network refused: no,") for line in refused) and
          linux.read("disk-nettoml4.out") == nettoml3, refused)
    check("uninstall has nothing to take out", "uninstall: done" not in out, linux.read("disk-uninstall.out"))
    check("an install over its root is refused: update is what renews it",
          "install over it: done" not in out and "nos-install update" in linux.read("disk-install.out"),
          linux.read("disk-install.out"))
    check("e2fsck finds the root clean after the update", "fsck6: clean" in out, out)

    m.boot("disk-two-nics", disk="whole.raw", two_nics=True)
    try:
        check("two NICs boot nos", m.wait_log("boot: complete", BOOT_TIMEOUT), m.text()[-3000:])
        version = m.wait_ssh(300)
        check("sshd answers through the second NIC", version == want,
              "%r, the image's VERSION says %r" % (version, want))
        if version is not None:
            rc, out = m.ssh(["net"])
            check("DHCP inside, a static default route and a private route outside",
                  rc == 0 and "eth0 settings: DHCP" in out and
                  "eth1 settings: 198.51.100.5/24, gateway 198.51.100.2" in out and
                  "default route: eth1" in out and "route 10.0.0.0/8 via 10.0.2.2 dev eth0" in out, out)
            rc, out = m.ssh(["ping", "10.0.2.2"])
            check("the inside DHCP network answers through eth0", rc == 0 and "5/5 received" in out, out)
            rc, out = m.ssh(["cat", "/etc/rc"])
            check("the installed sshd listens on the default interface", rc == 0 and "sshd start 22 nic=eth1" in out, out)
        m.ssh(["poweroff"], timeout=60)
        check("nos powers off with two NICs", m.wait_exit(120))
    finally:
        m.stop()


def nettoml_values(text):
    """/etc/net.toml's keys and values, as nos-install writes them: key =
    "value" or key = true, comments aside."""
    values = {}
    for line in text.splitlines():
        line = line.split("#", 1)[0].strip()
        if "=" in line:
            key, value = (part.strip() for part in line.split("=", 1))
            values[key] = value.strip('"')
    return values


def digest_of(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def release_dirs(tmp):
    """What the one-line installer fetches, as a release has it: the image and
    SHA256SUMS -- and a release whose sums are another image's."""
    image = os.path.join(tmp, "image.squashfs")
    for name, digest in (("release", digest_of(image)), ("release-bad", "0" * 64)):
        os.makedirs(os.path.join(tmp, name))
        shutil.copy(image, os.path.join(tmp, name, "nos-x86_64.squashfs"))
        with open(os.path.join(tmp, name, "SHA256SUMS"), "w") as f:
            f.write("%s  install.sh\n%s  nos-x86_64.squashfs\n" % ("1" * 64, digest))
    shutil.copy(os.path.join(ROOT, "scripts", "installer", "install.sh"), os.path.join(tmp, "install.sh"))


def run(tmp, image):
    linux = Linux(tmp)
    shutil.copy(image, os.path.join(tmp, "image.squashfs"))
    release_dirs(tmp)
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
    nettoml1 = linux.read("nettoml1.out")
    check("install writes /etc/net.toml: DHCP, given no network", nettoml_values(nettoml1) == {"dhcp": "true"},
          nettoml1)
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
            rc, out = m.ssh(["net"])
            check("eth0's settings are /etc/net.toml's: DHCP",
                  rc == 0 and "eth0 settings: DHCP, the lease's resolver (/etc/net.toml)" in out, out[-400:])
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
    check("update keeps the old kernel, modules, loaders, licenses and VERSION for a rollback",
          all(p in layout for p in ("kernel64.elf.prev", "modules.prev", "hvboot.prev", "CLOUDHV.fd.prev",
                                    "licenses.prev", "VERSION.prev")), layout)
    cloudhv_notice = linux.read("img-cloudhv-license.out")
    check("the image carries the CloudHv firmware's source and license notices",
          "edk2-stable202608" in cloudhv_notice and "BSD-2-Clause-Patent" in cloudhv_notice and
          "Copyright (c) 2016, Citrix Systems, Inc." in cloudhv_notice, cloudhv_notice[:300])
    notice = linux.read("img-license.out")
    check("the image carries the notice of the CRYPTOGAMS assembly its kernel and modules run, as its "
          "license asks of a binary", "CRYPTOGAMS by <appro@openssl.org>" in notice and
          "Redistributions in binary form" in notice, notice[:200])
    env2 = linux.read("grubenv2.out")
    check("update takes the UDP shell it was given, and arms a boot",
          "nos_cmdline=udpshell=9000" in env2 and "nos_next=nos" in env2, env2)
    nettoml2 = linux.read("nettoml2.out")
    check("update of an install from before /etc/net.toml writes it from the kernel parameters' words",
          nettoml_values(nettoml2) == {"ip": "10.0.2.15/24", "gw": "10.0.2.2", "dns": "10.0.2.3"} and
          "/etc/net.toml written from this machine's kernel parameters, ip=10.0.2.15/24 gw=10.0.2.2 dns=10.0.2.3"
          in out, nettoml2 + "".join(line + "\n" for line in out.splitlines() if "net.toml" in line))
    check("and keeps the words beside the UDP shell, for the kernel kept for a rollback, which does not read it",
          "nos_cmdline=udpshell=9000 ip=10.0.2.15/24 gw=10.0.2.2 dns=10.0.2.3" in env2.splitlines(), env2)
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
        rc, out = m.ssh(["net"])
        check("eth0's settings are the static address /etc/net.toml was written with",
              rc == 0 and "eth0 settings: 10.0.2.15/24, gateway 10.0.2.2, resolver 10.0.2.3 (/etc/net.toml)" in out,
              out[-400:])
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
    check("and eth0's settings",
          'network:     /etc/net.toml: ip = "10.0.2.15/24"; gw = "10.0.2.2"; dns = "10.0.2.3"' in status, status)
    env3 = linux.read("grubenv3.out")
    check("an update with every kernel on the partition reading /etc/net.toml takes the words out",
          "nos_cmdline=udpshell=9000" in env3.splitlines() and
          "the kernel parameters' ip=, gw=, dns= and dhcp= taken out" in linux.read("update3.out"),
          env3 + linux.read("update3.out")[-600:])
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
