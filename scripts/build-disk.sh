#!/bin/bash
# Build a bootable MBR disk image: GRUB in the boot sector, one ext4
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

# The commit the kernel says it is (`version`): worked out here, since the
# builder image has no git, as scripts/build-iso-docker.sh does
if [ -z "${GIT_DESC:-}" ]; then
    GIT_DESC="$(git -C "$PROJECT_ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
    if git -C "$PROJECT_ROOT" status --porcelain -uno 2>/dev/null | grep -q .; then
        GIT_DESC="$GIT_DESC-dirty"
    fi
fi

# Step 1: Build kernel inside Docker
docker build --platform linux/amd64 -t nos-builder "$PROJECT_ROOT"
docker run --platform linux/amd64 --rm -e "VERSION=${VERSION:-dev}" -e "GIT_DESC=$GIT_DESC" -v "$PROJECT_ROOT:/src" -w /src nos-builder bash -c 'make clean && make nocheck'

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
parted -s $IMG mkpart primary ext4 1MiB 100%
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

echo "Formatting partition 1 as ext4 (label nos)..."
# Its features named from none, as scripts/mkrootfs.sh and nos-install name
# them: what the ext4 of nos writes and GRUB 2.06 reads, nothing this mke2fs
# adds; its inode tables and journal written whole (docs/filesystems.md)
EXT4_FEATURES=none,has_journal,ext_attr,resize_inode,dir_index,filetype,extent,64bit,flex_bg,sparse_super,large_file,huge_file,dir_nlink,extra_isize,metadata_csum
mkfs.ext4 -q -F -L nos -b 4096 -O $EXT4_FEATURES \
    -E offset=$P1_OFFSET,lazy_itable_init=0,lazy_journal_init=0 -d $STAGE $IMG ${P1_SIZE_MB}M
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
# e2fsck takes the offset of the partition in the name of the image: it
# has no -E offset=, which it refused -- and so checked nothing -- while its
# exit code was ignored
e2fsck -f -n "$IMG?offset=$P1_OFFSET" > /tmp/e2fsck.log 2>&1 ||
    { cat /tmp/e2fsck.log; echo "e2fsck finds the new filesystem damaged"; exit 1; }

echo "Converting to qcow2..."
qemu-img convert -f raw -O qcow2 $IMG $QCOW
rm -f $IMG

echo "Done: nos.qcow2 (${SIZE_MB}MB, MBR, ext4 root labelled nos)"
'
