#!/bin/bash
# Build a bootable MBR disk image: GRUB in the boot sector, one ext2
# partition labelled "nos" holding /boot/kernel64.elf, /boot/grub and the
# root filesystem the kernel mounts read-write at boot (root=auto finds it
# by the label). Everything runs inside Docker, and nothing needs root:
# mke2fs populates the filesystem from a directory (-d) and writes it at an
# offset inside the image (-E offset=), GRUB's boot code is dd'ed in.
#
# Usage from host: scripts/build-disk.sh
#   SIZE_MB   image size (default 1024)
#   ROOTFS    a directory whose content goes into the root filesystem next
#             to /boot (optional; e.g. modules under lib/modules)
set -e

PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# Step 1: Build kernel inside Docker
docker build --platform linux/amd64 -t nos-builder "$PROJECT_ROOT"
docker run --platform linux/amd64 --rm -e "VERSION=${VERSION:-dev}" -v "$PROJECT_ROOT:/src" -w /src nos-builder bash -c 'make clean && make nocheck'

# Step 2: Build the disk image inside Docker
ROOTFS_MOUNT=""
if [ -n "${ROOTFS:-}" ]; then
    ROOTFS_ABS="$(cd "$ROOTFS" && pwd)"
    ROOTFS_MOUNT="-v $ROOTFS_ABS:/rootfs:ro"
fi

docker run --platform linux/amd64 --rm \
    -e "SIZE_MB=${SIZE_MB:-1024}" \
    -v "$PROJECT_ROOT:/src" $ROOTFS_MOUNT -w /src nos-builder \
    bash -c '
set -e

IMG=/src/nos.raw
QCOW=/src/nos.qcow2
P1_OFFSET=1048576     # 1 MiB - partition 1 start, the usual alignment
P1_SIZE_MB=$((SIZE_MB - 1))

echo "Creating ${SIZE_MB}MB raw disk image..."
rm -f $IMG
dd if=/dev/zero of=$IMG bs=1M count=0 seek=$SIZE_MB status=none

echo "Creating MBR partition table..."
parted -s $IMG mklabel msdos
parted -s $IMG mkpart primary ext2 1MiB 100%
parted -s $IMG set 1 boot on

echo "Staging the root filesystem..."
STAGE=$(mktemp -d)
if [ -d /rootfs ]; then
    # Plain copy: a macOS bind mount serves no extended attributes to keep
    cp -r /rootfs/. $STAGE/
fi
mkdir -p $STAGE/boot/grub $STAGE/lib/modules $STAGE/etc
cp /src/bin/kernel64.elf $STAGE/boot/kernel64.elf
cp /src/build/grub-disk.cfg $STAGE/boot/grub/grub.cfg
# GRUB modules so normal.mod etc. are available at boot
cp -r /usr/lib/grub/i386-pc $STAGE/boot/grub/

echo "Formatting partition 1 as ext2 (label nos)..."
# No htree index: the kernel writes directories linearly (see docs/filesystems.md)
mkfs.ext2 -q -F -L nos -b 4096 -O ^dir_index -E offset=$P1_OFFSET -d $STAGE $IMG ${P1_SIZE_MB}M
rm -rf $STAGE

# Custom GRUB core.img with a hardcoded root device: the default
# UUID-based search fails in QEMU.
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
dd if=/usr/lib/grub/i386-pc/boot.img of=$IMG bs=440 count=1 conv=notrunc status=none
dd if=/tmp/core.img of=$IMG bs=512 seek=1 conv=notrunc status=none

echo "Checking the filesystem..."
e2fsck -f -n -E offset=$P1_OFFSET $IMG > /dev/null 2>&1 || true

echo "Converting to qcow2..."
qemu-img convert -f raw -O qcow2 $IMG $QCOW
rm -f $IMG

echo "Done: nos.qcow2 (${SIZE_MB}MB, MBR, ext2 root labelled nos)"
'
