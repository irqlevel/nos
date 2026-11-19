#!/bin/bash
# tests/gpu-passthrough-test.py --bare-metal, on the machine with the GPU: nos
# put on a spare disk of the machine's and booted once, to give the GPU to a
# guest running NVIDIA's own driver -- the guest make-nvidia-root.sh made,
# kept under <dir>/nvidia -- and to go back to the machine's own system when
# that is done: its /etc/rc ends in a reboot, the boot is GRUB's one-shot
# (nos-install boot), and the kernel parameters given are meant to hold
# deadman= and the rest, so whatever becomes of nos the boot after it is the
# machine's own.
#
# The disk is named by its /dev/disk/by-id name -- Linux may swap nvme0 and
# nvme1 between boots -- and is either blank, then given a GPT of two
# partitions, "nos" (nos's root) and "nos-disklog" (the disk log's area,
# scripts/disklog.py's), or laid out so by an earlier run. Anything else is
# refused.
#
#   install-bare.sh <dir> <disk by-id> <image> <pci> <guest MiB> <secs> <netconsole ip:port> <kernel args...>
set -eu
D=$1
DISK_ID=$2
DISK=/dev/disk/by-id/$DISK_ID
IMAGE=$3
PCI=$4
MEM=$5
SECS=$6
NC=$7
shift 7
ARGS="$*"
N=$D/nvidia
G=$N/guest
ROOT_SIZE=64G
LOG_SIZE=64M
M=$D/mnt
trap 'echo "install-bare: failed at line $LINENO"' ERR

[ -b "$DISK" ] || { echo "install-bare: no disk $DISK"; exit 1; }
[ -e $N/guest.done ] && [ -e $N/samples.done ] || { echo "install-bare: no guest -- make-nvidia-root.sh first"; exit 1; }
DEV=$(readlink -f $DISK)
part() { lsblk -nro PATH,PARTLABEL $DEV | awk -v l="$1" '$2 == l { print $1 }'; }

if [ -z "$(lsblk -nro PATH $DEV | tail -n +2)" ]; then
    # No partitions: blank, or refused -- no signature of anything, and the
    # first MiB zeros
    if [ -n "$(wipefs -n $DEV)" ] || [ "$(dd if=$DEV bs=1M count=1 status=none | tr -d '\000' | wc -c)" != 0 ]; then
        echo "install-bare: $DISK has no partitions, but is not blank"
        exit 1
    fi
    printf 'label: gpt\nsize=%s, name=nos\nsize=%s, name=nos-disklog\n' $ROOT_SIZE $LOG_SIZE | sfdisk -q $DEV
    udevadm settle
    python3 -I $D/disklog.py format $(part nos-disklog)
fi
P1=$(part nos)
P2=$(part nos-disklog)
if [ -z "$P1" ] || [ -z "$P2" ]; then
    echo "install-bare: $DISK is laid out otherwise"
    exit 1
fi

mkdir -p $M/image $M/root
cleanup() {
    mountpoint -q $M/root && umount $M/root
    mountpoint -q $M/image && umount $M/image
    true
}
trap cleanup EXIT
mount -o loop,ro $IMAGE $M/image
$M/image/nos-install install $P1 --image $IMAGE --ssh-key /root/.ssh/authorized_keys --netconsole $NC --cmdline "$ARGS"

# The guest: its root as make-nvidia-root.sh leaves it, the samples and
# /init put in again, made into an image on nos's root, with the machine's
# own kernel -- the one NVIDIA's modules in it were built for
cp $D/nvidia-guest-init $G/init
chmod +x $G/init
rm -rf $G/opt/cuda-samples
mkdir -p $G/opt/cuda-samples
cp $N/samples/* $G/opt/cuda-samples/
mount $P1 $M/root
rm -f $M/root/guest.img
used=$(du -sm $G | cut -f1)
mke2fs -q -t ext4 -L guest -d $G $M/root/guest.img $((used * 13 / 10 + 256))M
cp /boot/vmlinuz-$(uname -r) $M/root/vmlinuz
cat > $M/root/etc/rc <<EOF
# tests/gpu-passthrough-test.py --bare-metal: nos gives the GPU to a guest
# running NVIDIA's own driver, then the machine goes back to its own system
insmod /lib/modules/sshd.ko
sshd start
insmod /lib/modules/hv.ko
hv on
hv boot /vmlinuz mem=$MEM cpus=2 secs=$SECS pci=$PCI disk=/guest.img cmdline=console=ttyS0 root=/dev/vda rw init=/init nokaslr
reboot
EOF
umount $M/root
$M/image/nos-install boot $P1
echo "install-bare: nos on $P1 ($DISK_ID), the disk log on $P2, armed for the next boot"
