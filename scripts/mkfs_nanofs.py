#!/usr/bin/env python3
"""Format partition 2 of a raw disk image as NanoFs."""

import struct
import sys
import os
import zlib

BLOCK_SIZE      = 4096
INODE_COUNT     = 1024
DATA_BLOCK_CNT  = 16384
INODE_START     = 1
DATA_START      = 1 + INODE_COUNT  # 1025
MAGIC           = 0x4E414E4F       # "NANO"
VERSION         = 1
MAX_BLOCKS      = 256
SECTOR_SIZE     = 512

INODE_TYPE_DIR  = 2

# Superblock field sizes (all little-endian)
#   u32 Magic, u32 Version, u8 Uuid[16], u32 Checksum,
#   u32 BlockSize, u32 InodeCount, u32 DataBlockCount,
#   u32 InodeStartBlock, u32 DataStartBlock
#   u8 InodeBitmap[128], u8 DataBitmap[2048], u8 Padding[...]
SB_FIXED_OFFSET = 0
SB_INODE_BM_OFFSET = 48
SB_DATA_BM_OFFSET  = 48 + 128
SB_CHECKSUM_OFFSET = 24

# Inode layout:
#   u32 Type, u32 Size, char Name[64], u32 ParentInode,
#   u32 Checksum, u32 DataChecksum, u32 Blocks[256], u8 Padding[...]
INODE_CHECKSUM_OFFSET = 76


def crc32(data: bytes) -> int:
    return zlib.crc32(data) & 0xFFFFFFFF


def build_superblock() -> bytearray:
    sb = bytearray(BLOCK_SIZE)
    uuid = os.urandom(16)
    struct.pack_into('<II16sIIIIII', sb, SB_FIXED_OFFSET,
                     MAGIC, VERSION, uuid,
                     0,  # Checksum placeholder
                     BLOCK_SIZE, INODE_COUNT, DATA_BLOCK_CNT,
                     INODE_START, DATA_START)
    # InodeBitmap: set bit 0 (root inode)
    sb[SB_INODE_BM_OFFSET] = 0x01
    # DataBitmap: set bit 0 (root dir data block)
    sb[SB_DATA_BM_OFFSET] = 0x01
    # Compute CRC32 with Checksum field zeroed
    struct.pack_into('<I', sb, SB_CHECKSUM_OFFSET, 0)
    checksum = crc32(bytes(sb))
    struct.pack_into('<I', sb, SB_CHECKSUM_OFFSET, checksum)
    return sb


def build_root_inode() -> bytearray:
    inode = bytearray(BLOCK_SIZE)
    # Type=dir, Size=0
    struct.pack_into('<II', inode, 0, INODE_TYPE_DIR, 0)
    # Name = "/" (64 bytes, zero-padded)
    inode[8] = ord('/')
    # ParentInode=0, Checksum=0(placeholder), DataChecksum=0
    struct.pack_into('<III', inode, 72, 0, 0, 0)
    # Blocks[0] = 0 (first data block)
    struct.pack_into('<I', inode, 84, 0)
    # Compute CRC32 with Checksum field zeroed
    struct.pack_into('<I', inode, INODE_CHECKSUM_OFFSET, 0)
    checksum = crc32(bytes(inode))
    struct.pack_into('<I', inode, INODE_CHECKSUM_OFFSET, checksum)
    return inode


def get_partition2_offset(img_path: str) -> int:
    with open(img_path, 'rb') as f:
        mbr = f.read(SECTOR_SIZE)
    # MBR partition entry 2 starts at offset 462 (446 + 16)
    entry = mbr[462:478]
    lba_start = struct.unpack_from('<I', entry, 8)[0]
    if lba_start == 0:
        print("ERROR: partition 2 not found in MBR", file=sys.stderr)
        sys.exit(1)
    return lba_start * SECTOR_SIZE


def main():
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} <raw-image>", file=sys.stderr)
        sys.exit(1)

    img = sys.argv[1]
    offset = get_partition2_offset(img)

    sb = build_superblock()
    root_inode = build_root_inode()
    zero_block = bytes(BLOCK_SIZE)

    with open(img, 'r+b') as f:
        f.seek(offset + 0 * BLOCK_SIZE)
        f.write(sb)
        f.seek(offset + INODE_START * BLOCK_SIZE)
        f.write(root_inode)
        f.seek(offset + DATA_START * BLOCK_SIZE)
        f.write(zero_block)

    print(f"NanoFs: formatted partition 2 at offset {offset} ({offset // (1024*1024)} MiB)")


if __name__ == '__main__':
    main()
