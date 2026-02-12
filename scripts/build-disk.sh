#!/bin/bash
# Build a bootable MBR disk image with GRUB + kernel.
# Must be run inside Docker with --privileged (for losetup/mount).
# Usage from host: scripts/build-disk.sh
set -e

PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# Step 1: Build kernel inside Docker
docker build --platform linux/amd64 -t nos-builder "$PROJECT_ROOT"
docker run --platform linux/amd64 --rm -e VERSION -v "$PROJECT_ROOT:/src" -w /src nos-builder bash -c 'make clean && make nocheck'

# Step 2: Build disk image inside Docker (needs --privileged for loop mounts)
docker run --platform linux/amd64 --rm --privileged \
    -v "$PROJECT_ROOT:/src" -w /src nos-builder \
    bash -c '
set -e

IMG=/src/nos.raw
QCOW=/src/nos.qcow2
SIZE=1024  # MB
P1_OFFSET=1048576     # 1 MiB  - partition 1 start
P1_SIZELIMIT=33554432 # 32 MiB - partition 1 size

echo "Creating ${SIZE}MB raw disk image..."
dd if=/dev/zero of=$IMG bs=1M count=$SIZE status=none

echo "Creating MBR partition table..."
parted -s $IMG mklabel msdos
parted -s $IMG mkpart primary ext2 1MiB 33MiB
parted -s $IMG mkpart primary 33MiB 100%
parted -s $IMG set 1 boot on

# Whole-disk loop device (for grub-install to write MBR boot code)
echo "Setting up whole-disk loop device..."
LOOP_DISK=$(losetup --show -f $IMG)
echo "Whole-disk loop: $LOOP_DISK"

# Partition 1 loop device (with offset + sizelimit)
echo "Setting up partition 1 loop device..."
LOOP_P1=$(losetup --show -f --offset $P1_OFFSET --sizelimit $P1_SIZELIMIT $IMG)
echo "Partition 1 loop: $LOOP_P1"

echo "Formatting partition 1 as ext2..."
mkfs.ext2 -q $LOOP_P1

echo "Mounting partition 1..."
MNTDIR=$(mktemp -d)
mount $LOOP_P1 $MNTDIR

mkdir -p $MNTDIR/boot/grub
cp /src/bin/kernel64.elf $MNTDIR/boot/kernel64.elf
cp /src/build/grub-disk.cfg $MNTDIR/boot/grub/grub.cfg

# Copy GRUB modules so normal.mod etc. are available at boot
cp -r /usr/lib/grub/i386-pc $MNTDIR/boot/grub/

# Build custom GRUB core.img with hardcoded root device.
# This avoids the default UUID-based search which fails in QEMU.
cat > /tmp/grub-early.cfg << EOFCFG
serial --unit=0 --speed=115200
terminal_input serial console
terminal_output serial console
set root=(hd0,msdos1)
set prefix=(hd0,msdos1)/boot/grub
EOFCFG

echo "Building GRUB core image..."
grub-mkimage -O i386-pc -o /tmp/core.img \
    -c /tmp/grub-early.cfg \
    -p "(hd0,msdos1)/boot/grub" \
    biosdisk part_msdos ext2 normal multiboot2 serial terminal

echo "Installing GRUB boot sector and core image..."
dd if=/usr/lib/grub/i386-pc/boot.img of=$LOOP_DISK bs=440 count=1 conv=notrunc
dd if=/tmp/core.img of=$LOOP_DISK bs=512 seek=1 conv=notrunc

echo "Unmounting..."
umount $MNTDIR
rmdir $MNTDIR
losetup -d $LOOP_P1
losetup -d $LOOP_DISK

echo "Converting to qcow2..."
qemu-img convert -f raw -O qcow2 $IMG $QCOW
rm -f $IMG

echo "Done: nos.qcow2 (${SIZE}MB, MBR, 2 partitions)"
'
