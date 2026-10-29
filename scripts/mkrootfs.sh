#!/bin/bash
# Make a root filesystem image for nos: ext4, as new installs get one -- or,
# with FSTYPE=ext2, ext2, as the roots made before ext4 are.
#
#   [FSTYPE=ext2] scripts/mkrootfs.sh <image> <size-MiB> [dir] [block-size]
#
# The image gets the label "nos", which is what root=auto looks for. ext4's
# features are named one by one from none (EXT4_FEATURES, as nos-install
# names them), so that what this machine's mke2fs.conf would add -- orphan_file
# and metadata_csum_seed among them, which nos mounts read-only and GRUB 2.06
# cannot read -- is not there; its inode tables and its journal are written
# whole, so that nothing an earlier use of the blocks left is ever read as
# either. ext2 gets no htree directory index, since ext2's driver writes
# directories linearly. dir, if given, is copied in as the root of the
# filesystem (this is mke2fs -d, no root privileges needed). block-size
# defaults to 4096; the smoke tests use 1024, so that their boot's fstest
# reaches deeper extent trees on ext4 -- and ext2's double- and
# triple-indirect blocks on ext2.
#
# Runs mke2fs natively when e2fsprogs is installed, else inside the
# nos-builder Docker image (macOS).
set -eu

if [ $# -lt 2 ]; then
    echo "usage: [FSTYPE=ext2] $0 <image> <size-MiB> [dir] [block-size]" >&2
    exit 1
fi

IMG=$1
SIZE=$2
DIR=${3:-}
BS=${4:-4096}
FSTYPE=${FSTYPE:-ext4}

# What nos's ext4 writes, and GRUB 2.06 reads: e2fsprogs 1.46's ext4, but
# for nothing a newer one adds. nos-install makes new installs with the same.
EXT4_FEATURES=none,has_journal,ext_attr,resize_inode,dir_index,filetype,extent,64bit,flex_bg,sparse_super,large_file,huge_file,dir_nlink,extra_isize,metadata_csum

# Options mke2fs takes: the filesystem, force on a regular file, quiet, our
# label, block size, its features
mkfs_opts() {
    case "$FSTYPE" in
        ext4) echo "-t ext4 -F -q -L nos -b $BS -O $EXT4_FEATURES -E lazy_itable_init=0,lazy_journal_init=0" ;;
        ext2) echo "-t ext2 -F -q -L nos -b $BS -O ^dir_index" ;;
        *) echo "mkrootfs: FSTYPE is ext4 or ext2, not $FSTYPE" >&2; exit 1 ;;
    esac
}
OPTS=$(mkfs_opts)

if command -v mke2fs >/dev/null 2>&1; then
    rm -f "$IMG"
    dd if=/dev/zero of="$IMG" bs=1M count=0 seek="$SIZE" status=none
    if [ -n "$DIR" ]; then
        mke2fs $OPTS -d "$DIR" "$IMG"
    else
        mke2fs $OPTS "$IMG"
    fi
else
    IMG_DIR=$(cd "$(dirname "$IMG")" && pwd)
    IMG_NAME=$(basename "$IMG")
    rm -f "$IMG"
    dd if=/dev/zero of="$IMG" bs=1M count=0 seek="$SIZE" status=none
    if [ -n "$DIR" ]; then
        # Staged through a directory inside the container: mke2fs -d copies
        # extended attributes, which a macOS bind mount does not serve
        DIR_ABS=$(cd "$DIR" && pwd)
        docker run --platform linux/amd64 --rm \
            -v "$IMG_DIR:/img" -v "$DIR_ABS:/rootdir:ro" nos-builder \
            bash -c "cp -r /rootdir /tmp/rootdir && mke2fs $OPTS -d /tmp/rootdir /img/$IMG_NAME"
    else
        docker run --platform linux/amd64 --rm \
            -v "$IMG_DIR:/img" nos-builder \
            mke2fs $OPTS "/img/$IMG_NAME"
    fi
fi

echo "mkrootfs: $IMG (${SIZE} MiB, $FSTYPE, label nos, block size $BS)"
