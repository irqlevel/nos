#!/bin/bash
# The GPU and an NVMe given to one guest on the machine's metal
# (docs/real-hardware.md, Hertz), run by hand on that machine from its own
# system, as root, once install-bare.sh has laid out nos's disk -- an NVMe
# of its own -- and the guest is made there. That disk is what the guest
# gets: nos boots instead from two new partitions in the free space at the
# end of the machine's system disk, named nos and nos-disklog, and
# guestpci= keeps nos's drivers off its old disk. The guest boots its root
# from it: nvme-guest-init, in an initramfs of busybox and the machine's
# kernel's nvme modules, reads the first GiB of nos's old root, writes 256
# MiB of random to the partition named nos-scratch and reads it back past
# the page cache, then mounts the partition named nos-guest and hands over
# to the GPU's test (nvidia-guest-init). Then the machine goes back to its
# own system, as install-bare.sh's does, where --check sums what the guest
# read and wrote and reads the boot's disk log.
#
# The system disk is only added to: its table dumped into <dir> first, two
# partitions made in the free space at its end and nothing else of it
# touched. nos's disk gains nos-guest and nos-scratch after its own two.
# What is there already is kept, so that a second run reuses it.
#
#   install-bare-nvme.sh <dir> <system disk by-id> <nos's disk by-id> <image> <gpu pci> <guest MiB> <secs> <netconsole ip:port> <kernel args...>
#   install-bare-nvme.sh --check <dir> <system disk by-id> <nos's disk by-id>
#
# <dir> is install-bare.sh's, with nvidia-guest-init, nvme-guest-init and
# scripts/disklog.py beside the guest. What the boot did is read off the
# netconsole collector (scripts/netconsole.py on another host), and off the
# disk log once the machine is back.
set -euo pipefail
CHECK=0
if [ "${1:-}" = --check ]; then
    CHECK=1
    shift
fi
D=$1
SYS=/dev/disk/by-id/$2
NOSD=/dev/disk/by-id/$3
N=$D/nvidia
G=$N/guest
SECTOR_MIB=2048
ALIGN=$SECTOR_MIB
ROOT_MIN=$((512 * SECTOR_MIB))
ROOT_MAX=$((2048 * SECTOR_MIB))
LOG_SECTORS=$((64 * SECTOR_MIB))
M=$D/mnt
trap 'echo "install-bare-nvme: failed at line $LINENO"' ERR

[ -b "$SYS" ] && [ -b "$NOSD" ] || { echo "install-bare-nvme: no disk $SYS or $NOSD"; exit 1; }
SYSDEV=$(readlink -f $SYS)
NOSDEV=$(readlink -f $NOSD)
[ "$SYSDEV" != "$NOSDEV" ] || { echo "install-bare-nvme: the two disks are one"; exit 1; }
part() { lsblk -nro PATH,PARTLABEL $1 | awk -v l="$2" '$2 == l { print $1 }'; }

if [ $CHECK = 1 ]; then
    echo "nos-scratch, its first 256 MiB: $(dd if=$(part $NOSDEV nos-scratch) bs=1M count=256 status=none | sha256sum | cut -d' ' -f1)"
    echo "nos's old root, its first GiB: $(dd if=$(part $NOSDEV nos) bs=1M count=1024 status=none | sha256sum | cut -d' ' -f1), before the boot $(cat $D/nvme-old-root.sha256)"
    python3 -I $D/disklog.py read "$(part $SYSDEV nos-disklog)" > $D/disklog-nvme.txt
    echo "the disk log: $(wc -l < $D/disklog-nvme.txt) lines in $D/disklog-nvme.txt"
    exit 0
fi

IMAGE=$4
GPU=$5
MEM=$6
SECS=$7
NC=$8
shift 8
# nos's disk as nos and the guest's PCI will name it: bus:device.function
NVME=$(basename "$(readlink -f /sys/block/${NOSDEV##*/}/device/device)")
NVME=${NVME#0000:}
[[ $NVME =~ ^[0-9a-f]{2}:[0-9a-f]{2}\.[0-7]$ ]] || { echo "install-bare-nvme: $NOSD is no NVMe of PCI domain 0 ($NVME)"; exit 1; }
ARGS="$* guestpci=$NVME"
[ -e $N/guest.done ] && [ -e $N/samples.done ] || { echo "install-bare-nvme: no guest -- make-nvidia-root.sh first"; exit 1; }
OLD=$(part $NOSDEV nos)
[ -n "$OLD" ] && [ -n "$(part $NOSDEV nos-disklog)" ] || { echo "install-bare-nvme: $NOSD is not laid out by install-bare.sh"; exit 1; }

# The system disk: nos's root and disk log in the free space at its end
if [ -z "$(part $SYSDEV nos)" ]; then
    read -r start end < <(sfdisk -F $SYSDEV | awk '$1 ~ /^[0-9]+$/ { s = $1; e = $2 } END { print s, e }')
    [ -n "${start:-}" ] || { echo "install-bare-nvme: $SYS has no free space"; exit 1; }
    first=$(( (start + ALIGN - 1) / ALIGN * ALIGN ))
    root=$(( (end + 1 - first - LOG_SECTORS) / ALIGN * ALIGN ))
    [ $root -ge $ROOT_MIN ] || { echo "install-bare-nvme: $SYS's free space at its end is under $((ROOT_MIN / SECTOR_MIB + 64)) MiB"; exit 1; }
    [ $root -le $ROOT_MAX ] || root=$ROOT_MAX
    [ -e $D/system-disk-before.sfdisk ] || sfdisk --dump $SYSDEV > $D/system-disk-before.sfdisk
    printf 'start=%s, size=%s, name=nos\nstart=%s, size=%s, name=nos-disklog\n' \
        $first $root $((first + root)) $LOG_SECTORS | sfdisk -q --append --no-reread $SYSDEV
    udevadm settle
    # The kernel told of them where the disk is in use: partx adds what it
    # does not have yet, and refuses -- harmlessly -- what it has
    [ -n "$(part $SYSDEV nos)" ] || partx -a $SYSDEV || true
    udevadm settle
    python3 -I $D/disklog.py format "$(part $SYSDEV nos-disklog)"
fi
P=$(part $SYSDEV nos)
L=$(part $SYSDEV nos-disklog)
[ -n "$P" ] && [ -n "$L" ] || { echo "install-bare-nvme: $SYS's partitions for nos not found"; exit 1; }

# nos's disk: the guest's root and a scratch, after nos's two
if [ -z "$(part $NOSDEV nos-guest)" ]; then
    printf 'size=8GiB, name=nos-guest\nsize=1GiB, name=nos-scratch\n' | sfdisk -q --append $NOSDEV
    udevadm settle
fi
GP=$(part $NOSDEV nos-guest)
SP=$(part $NOSDEV nos-scratch)
[ -n "$GP" ] && [ -n "$SP" ] || { echo "install-bare-nvme: $NOSD's partitions for the guest not found"; exit 1; }

# The guest's root as make-nvidia-root.sh leaves it, the samples and the
# GPU's test as its /init put in again, onto nos-guest; the scratch with no
# pattern on it, and what the guest's read of nos's old root has to sum to
cp $D/nvidia-guest-init $G/init
chmod +x $G/init
rm -rf $G/opt/cuda-samples
mkdir -p $G/opt/cuda-samples
cp $N/samples/* $G/opt/cuda-samples/
mke2fs -q -F -t ext4 -L nos-guest -d $G $GP
dd if=/dev/zero of=$SP bs=1M count=256 conv=fsync status=none
dd if=$OLD bs=1M count=1024 status=none | sha256sum | cut -d' ' -f1 > $D/nvme-old-root.sha256

# The guest's initramfs: busybox, every module nvme needs in the order
# modprobe loads them, the init
R=$D/nvme-initrd
rm -rf $R
mkdir -p $R/bin $R/lib/nvme $R/dev $R/proc $R/sys $R/tmp $R/newroot
cp "$(command -v busybox)" $R/bin/busybox
file -L $R/bin/busybox | grep -q "statically linked" || { echo "install-bare-nvme: busybox is not static"; exit 1; }
cp $D/nvme-guest-init $R/init
chmod +x $R/init
mknod -m 600 $R/dev/console c 5 1
: > $R/lib/nvme/order
modprobe --show-depends nvme | awk '$1 == "insmod" { print $2 }' > $D/nvme-modules.txt
[ -s $D/nvme-modules.txt ] || { echo "install-bare-nvme: modprobe names no module for nvme"; exit 1; }
while read -r f; do
    m=$(basename "$f" .zst)
    case "$f" in
        *.zst) zstd -q -d -f -o $R/lib/nvme/$m "$f" ;;
        *) cp "$f" $R/lib/nvme/$m ;;
    esac
    echo $m >> $R/lib/nvme/order
done < $D/nvme-modules.txt
(cd $R && find . | cpio -o -H newc --quiet | gzip -9) > $D/nvme-initrd.cpio.gz

mkdir -p $M/image $M/root
cleanup() {
    mountpoint -q $M/root && umount $M/root
    mountpoint -q $M/image && umount $M/image
    true
}
trap cleanup EXIT
mount -o loop,ro $IMAGE $M/image
$M/image/nos-install install $P --image $IMAGE --ssh-key /root/.ssh/authorized_keys --netconsole $NC --cmdline "$ARGS"
mount $P $M/root
cp /boot/vmlinuz-$(uname -r) $M/root/vmlinuz
cp $D/nvme-initrd.cpio.gz $M/root/nvme-initrd.cpio.gz
cat > $M/root/etc/rc <<EOF
# tests/gpu-passthrough/install-bare-nvme.sh: nos gives the GPU and its own
# old disk, an NVMe, to one guest, which boots its root from the NVMe and
# runs NVIDIA's driver on the GPU; then the machine goes back to its own
# system
insmod /lib/modules/sshd.ko
sshd start
insmod /lib/modules/hv.ko
hv on
hv pci
hv boot /vmlinuz initrd=/nvme-initrd.cpio.gz mem=$MEM cpus=2 secs=$SECS pci=$GPU pci=$NVME cmdline=console=ttyS0 rdinit=/init nokaslr
reboot
EOF
umount $M/root
$M/image/nos-install boot $P
echo "install-bare-nvme: nos on $P, the disk log on $L, the guest gets $GPU and $NVME (its root $GP, scratch $SP); armed for the next boot"
