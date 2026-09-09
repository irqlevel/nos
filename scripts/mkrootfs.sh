#!/bin/bash
# Make an ext2 root filesystem image for nos.
#
#   scripts/mkrootfs.sh <image> <size-MiB> [dir] [block-size]
#
# The image gets the label "nos", which is what root=auto looks for, and no
# htree directory index, since the kernel's ext2 driver writes directories
# linearly. dir, if given, is copied in as the root of the filesystem (this
# is mke2fs -d, no root privileges needed). block-size defaults to 4096; the
# smoke tests use 1024 so a 300 KiB file reaches the double-indirect blocks.
#
# Runs mke2fs natively when e2fsprogs is installed, else inside the
# nos-builder Docker image (macOS).
set -eu

if [ $# -lt 2 ]; then
    echo "usage: $0 <image> <size-MiB> [dir] [block-size]" >&2
    exit 1
fi

IMG=$1
SIZE=$2
DIR=${3:-}
BS=${4:-4096}

# Options mke2fs takes: force on a regular file, quiet, our label, block
# size, no dir_index, populate from DIR
mkfs_opts() {
    echo "-F -q -L nos -b $BS -O ^dir_index"
}

if command -v mkfs.ext2 >/dev/null 2>&1; then
    rm -f "$IMG"
    dd if=/dev/zero of="$IMG" bs=1M count=0 seek="$SIZE" status=none
    if [ -n "$DIR" ]; then
        mkfs.ext2 $(mkfs_opts) -d "$DIR" "$IMG"
    else
        mkfs.ext2 $(mkfs_opts) "$IMG"
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
            bash -c "cp -r /rootdir /tmp/rootdir && mkfs.ext2 $(mkfs_opts) -d /tmp/rootdir /img/$IMG_NAME"
    else
        docker run --platform linux/amd64 --rm \
            -v "$IMG_DIR:/img" nos-builder \
            mkfs.ext2 $(mkfs_opts) "/img/$IMG_NAME"
    fi
fi

echo "mkrootfs: $IMG (${SIZE} MiB, ext2, label nos, block size $BS)"
