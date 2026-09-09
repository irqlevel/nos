#include "ext2.h"

#include <lib/stdlib.h>
#include <mm/new.h>
#include <include/const.h>
#include <block/block_device.h>
#include <kernel/trace.h>
#include <kernel/time.h>

namespace Kernel
{

/* Bitmaps use little-endian bit order: bit i of the map is bit (i % 8) of
   byte (i / 8), the same numbering the x86 bit instructions use. */
static inline bool BitmapTest(const u8* map, ulong bit)
{
    return (map[bit / 8] & (u8)(1u << (bit % 8))) != 0;
}

static inline void BitmapSet(u8* map, ulong bit)
{
    map[bit / 8] = (u8)(map[bit / 8] | (u8)(1u << (bit % 8)));
}

static inline void BitmapClear(u8* map, ulong bit)
{
    map[bit / 8] = (u8)(map[bit / 8] & (u8)~(1u << (bit % 8)));
}

static inline ulong DirEntrySize(ulong nameLen)
{
    return Stdlib::RoundUp(Ext2DirEntryHeader + nameLen, Ext2DirEntryAlign);
}

Ext2Fs::Ext2Fs(BlockDevice* dev)
    : Dev(dev)
    , Super(nullptr)
    , GroupDescs(nullptr)
    , GdtBlocks(0)
    , GdtBlock(0)
    , BlockSize(0)
    , GroupCount(0)
    , InodeSize(128)
    , PtrsPerBlock(0)
    , RootNode(nullptr)
    , Mounted(false)
    , MetaDirty(false)
    , TmpBlock(nullptr)
    , DataBuf(nullptr)
    , IndBuf(nullptr)
    , IndBufBlock(0)
    , DindBuf(nullptr)
    , DindBufBlock(0)
    , BitmapBuf(nullptr)
    , BitmapBlock(0)
    , BitmapDirty(false)
{
}

Ext2Fs::~Ext2Fs()
{
    Unmount();
}

const char* Ext2Fs::GetName()
{
    return "ext2";
}

void Ext2Fs::GetInfo(char* buf, ulong bufSize)
{
    if (buf == nullptr || bufSize == 0)
        return;

    buf[0] = '\0';
    if (Dev == nullptr)
        return;

    if (Super != nullptr && Super->VolumeName[0] != '\0')
    {
        char label[sizeof(Super->VolumeName) + 1];
        Stdlib::MemCpy(label, Super->VolumeName, sizeof(Super->VolumeName));
        label[sizeof(Super->VolumeName)] = '\0';
        Stdlib::SnPrintf(buf, bufSize, "%s label=%s", Dev->GetName(), label);
    }
    else
    {
        Stdlib::SnPrintf(buf, bufSize, "%s", Dev->GetName());
    }
}

BlockDevice* Ext2Fs::GetDevice()
{
    return Dev;
}

VNode* Ext2Fs::GetRoot()
{
    return RootNode;
}

/* --- Block I/O --- */

bool Ext2Fs::ReadBlock(u32 blockNum, void* buf)
{
    if (Dev == nullptr)
        return false;

    if (blockNum >= Super->BlockCount)
    {
        Trace(0, "Ext2Fs: read of block %u beyond %u", (ulong)blockNum, (ulong)Super->BlockCount);
        return false;
    }

    u32 sectorsPerBlock = BlockSize / (u32)Dev->GetSectorSize();
    u64 startSector = (u64)blockNum * sectorsPerBlock;
    return Dev->ReadSectors(startSector, buf, sectorsPerBlock);
}

bool Ext2Fs::WriteBlock(u32 blockNum, const void* buf, bool fua)
{
    if (Dev == nullptr)
        return false;

    if (blockNum >= Super->BlockCount)
    {
        Trace(0, "Ext2Fs: write of block %u beyond %u", (ulong)blockNum, (ulong)Super->BlockCount);
        return false;
    }

    u32 sectorsPerBlock = BlockSize / (u32)Dev->GetSectorSize();
    u64 startSector = (u64)blockNum * sectorsPerBlock;
    return Dev->WriteSectors(startSector, buf, sectorsPerBlock, fua);
}

/* A block number read off the disk (an inode or indirect block pointer)
   must land inside the filesystem and past the boot block; anything else
   is corruption, and following it would read or overwrite metadata. */
bool Ext2Fs::IsDataBlock(u32 blockNum)
{
    return blockNum >= Super->FirstDataBlock && blockNum < Super->BlockCount;
}

/* --- Superblock and group descriptors --- */

/* The superblock starts at byte 1024 of the partition. For sector sizes
   up to 1024 that is a whole-sector offset; for larger sectors (4096) it
   sits inside the first sector. scratch is a page-aligned page. */
bool Ext2Fs::ReadSuperBlock(BlockDevice* dev, u8* scratch, Ext2SuperBlock* out)
{
    u32 sectorSize = (u32)dev->GetSectorSize();
    if (sectorSize == 0 || sectorSize > Const::PageSize)
    {
        Trace(0, "Ext2Fs: unsupported sector size %u", (ulong)sectorSize);
        return false;
    }

    u32 sbSectorStart = Ext2SuperBlockOffset / sectorSize;
    u32 sbOffsetInSector = Ext2SuperBlockOffset % sectorSize;
    u32 sbSectorCount = (sbOffsetInSector + (u32)sizeof(Ext2SuperBlock) + sectorSize - 1) / sectorSize;

    Stdlib::MemSet(scratch, 0, Const::PageSize);
    if (!dev->ReadSectors(sbSectorStart, scratch, sbSectorCount))
    {
        Trace(0, "Ext2Fs: failed to read superblock");
        return false;
    }

    Stdlib::MemCpy(out, scratch + sbOffsetInSector, sizeof(Ext2SuperBlock));
    return true;
}

bool Ext2Fs::Probe(BlockDevice* dev, Ext2Identity& id)
{
    if (dev == nullptr)
        return false;

    u8* scratch = static_cast<u8*>(Mm::Alloc(Const::PageSize, 0));
    if (scratch == nullptr)
        return false;

    Ext2SuperBlock* sb = new (Mm::NoThrow) Ext2SuperBlock();
    if (sb == nullptr)
    {
        Mm::Free(scratch);
        return false;
    }

    bool ok = ReadSuperBlock(dev, scratch, sb) && sb->Magic == Ext2Magic && sb->RevLevel >= 1;
    if (ok)
    {
        Stdlib::MemCpy(id.Uuid, sb->Uuid, sizeof(id.Uuid));
        Stdlib::MemCpy(id.Label, sb->VolumeName, sizeof(sb->VolumeName));
        id.Label[sizeof(sb->VolumeName)] = '\0';
    }

    delete sb;
    Mm::Free(scratch);
    return ok;
}

/* The primary superblock only. Linux does the same on an ordinary write;
   the backups are refreshed by e2fsck and resize2fs. */
bool Ext2Fs::WriteSuper()
{
    u32 sbBlock = Ext2SuperBlockOffset / BlockSize;
    u32 sbOffset = Ext2SuperBlockOffset % BlockSize;

    if (!ReadBlock(sbBlock, TmpBlock))
    {
        Trace(0, "Ext2Fs: read superblock block failed");
        return false;
    }

    Stdlib::MemCpy(TmpBlock + sbOffset, Super, sizeof(Ext2SuperBlock));
    if (!WriteBlock(sbBlock, TmpBlock, true))
    {
        Trace(0, "Ext2Fs: write superblock failed");
        return false;
    }
    return true;
}

bool Ext2Fs::WriteGroupDescs()
{
    const u8* table = reinterpret_cast<const u8*>(GroupDescs);
    for (ulong i = 0; i < GdtBlocks; i++)
    {
        /* Through TmpBlock: a block-sized slice of the table is not
           page-aligned for block sizes under a page, and DMA needs it so */
        Stdlib::MemCpy(TmpBlock, table + i * BlockSize, BlockSize);
        if (!WriteBlock(GdtBlock + (u32)i, TmpBlock, true))
        {
            Trace(0, "Ext2Fs: write group desc block %u failed", (ulong)i);
            return false;
        }
    }
    return true;
}

/* Put the in-memory free counts on disk: group descriptors first, then the
   superblock that summarises them. */
bool Ext2Fs::CommitMeta()
{
    if (!MetaDirty)
        return true;

    Super->WriteTime = (u32)GetWallTimeSecs();
    if (!WriteGroupDescs() || !WriteSuper())
        return false;

    MetaDirty = false;
    return true;
}

u32 Ext2Fs::BlocksInGroup(u32 group)
{
    u32 first = Super->FirstDataBlock + group * Super->BlocksPerGroup;
    if (first >= Super->BlockCount)
        return 0;
    u32 left = Super->BlockCount - first;
    return (left < Super->BlocksPerGroup) ? left : Super->BlocksPerGroup;
}

bool Ext2Fs::Mount()
{
    if (Mounted)
    {
        Trace(0, "Ext2Fs: already mounted");
        return false;
    }

    if (Dev == nullptr)
    {
        Trace(0, "Ext2Fs: no device");
        return false;
    }

    TmpBlock = static_cast<u8*>(Mm::Alloc(Const::PageSize, 0));
    DataBuf = static_cast<u8*>(Mm::Alloc(Const::PageSize, 0));
    IndBuf = static_cast<u8*>(Mm::Alloc(Const::PageSize, 0));
    DindBuf = static_cast<u8*>(Mm::Alloc(Const::PageSize, 0));
    BitmapBuf = static_cast<u8*>(Mm::Alloc(Const::PageSize, 0));
    if (TmpBlock == nullptr || DataBuf == nullptr || IndBuf == nullptr ||
        DindBuf == nullptr || BitmapBuf == nullptr)
    {
        Trace(0, "Ext2Fs: alloc block buffers failed");
        goto fail;
    }

    Super = new (Mm::NoThrow) Ext2SuperBlock();
    if (Super == nullptr)
    {
        Trace(0, "Ext2Fs: alloc superblock failed");
        goto fail;
    }

    if (!ReadSuperBlock(Dev, TmpBlock, Super))
        goto fail;

    if (Super->Magic != Ext2Magic)
    {
        Trace(0, "Ext2Fs: bad magic 0x%p", (ulong)Super->Magic);
        goto fail;
    }

    /* Refuse formats this driver cannot parse: mounting e.g. an ext4 image
       (same magic) would return unrelated disk blocks as file data with
       success status. Rev 0 lacks the required filetype dirent feature. */
    if (Super->RevLevel < 1 ||
        (Super->FeatureIncompat & ~Ext2IncompatSupported) != 0 ||
        (Super->FeatureIncompat & Ext2IncompatFileType) == 0)
    {
        Trace(0, "Ext2Fs: unsupported rev %u / incompat features 0x%p",
              (ulong)Super->RevLevel, (ulong)Super->FeatureIncompat);
        goto fail;
    }

    /* LogBlockSize is raw disk data feeding a shift: bound it before
       shifting (a count >= 32 is UB, and the x86-masked result could pass
       the range check below for an incoherent image). */
    if (Super->LogBlockSize > Ext2MaxLogBlockSize)
    {
        Trace(0, "Ext2Fs: unsupported log block size %u", (ulong)Super->LogBlockSize);
        goto fail;
    }

    BlockSize = 1024u << Super->LogBlockSize;
    if (BlockSize < 1024 || BlockSize > Const::PageSize || BlockSize < (u32)Dev->GetSectorSize() ||
        (BlockSize % (u32)Dev->GetSectorSize()) != 0)
    {
        Trace(0, "Ext2Fs: unsupported block size %u (sector size %u)",
              (ulong)BlockSize, (ulong)Dev->GetSectorSize());
        goto fail;
    }
    PtrsPerBlock = BlockSize / sizeof(u32);

    InodeSize = 128;
    if (Super->InodeSize > 0)
        InodeSize = Super->InodeSize;

    /* InodeSize is on-disk data. ReadInode copies a fixed sizeof(Ext2Inode)
       struct from an offset within a single BlockSize buffer, so InodeSize
       must be at least that size and divide BlockSize evenly -- otherwise a
       crafted image could make the copy straddle the end of TmpBlock. */
    if (InodeSize < sizeof(Ext2Inode) || (BlockSize % InodeSize) != 0)
    {
        Trace(0, "Ext2Fs: unsupported inode size %u (block size %u)",
              (ulong)InodeSize, (ulong)BlockSize);
        goto fail;
    }

    /* Both fields are divisors (here and in ReadInode); a corrupt image with
       either at 0 would raise a division exception. */
    if (Super->BlocksPerGroup == 0 || Super->InodesPerGroup == 0 ||
        Super->InodesPerGroup > (BlockSize * 8) || Super->BlocksPerGroup > (BlockSize * 8))
    {
        Trace(0, "Ext2Fs: bad blocks/inodes per group %u/%u",
              (ulong)Super->BlocksPerGroup, (ulong)Super->InodesPerGroup);
        goto fail;
    }

    if (Super->FirstDataBlock >= Super->BlockCount)
    {
        Trace(0, "Ext2Fs: first data block %u beyond %u",
              (ulong)Super->FirstDataBlock, (ulong)Super->BlockCount);
        goto fail;
    }

    GroupCount = (Super->BlockCount - Super->FirstDataBlock + Super->BlocksPerGroup - 1) / Super->BlocksPerGroup;
    if (GroupCount == 0)
    {
        Trace(0, "Ext2Fs: zero groups");
        goto fail;
    }

    /* Read group descriptor table.
       It starts at block (FirstDataBlock + 1).
       For 1024-byte blocks, FirstDataBlock is 1 (block 0 is boot, block 1 is sb).
       For 4096-byte blocks, FirstDataBlock is 0 (sb is in block 0 at offset 1024). */
    {
        GdtBlock = Super->FirstDataBlock + 1;
        ulong gdtSize = (ulong)GroupCount * sizeof(Ext2GroupDesc);
        GdtBlocks = (gdtSize + BlockSize - 1) / BlockSize;

        GroupDescs = static_cast<Ext2GroupDesc*>(Mm::Alloc(GdtBlocks * BlockSize, 0));
        if (GroupDescs == nullptr)
        {
            Trace(0, "Ext2Fs: alloc group descs failed");
            goto fail;
        }

        u8* gdtBuf = reinterpret_cast<u8*>(GroupDescs);
        for (ulong i = 0; i < GdtBlocks; i++)
        {
            if (!ReadBlock(GdtBlock + (u32)i, TmpBlock))
            {
                Trace(0, "Ext2Fs: failed to read group desc block %u", (ulong)(GdtBlock + i));
                goto fail;
            }
            Stdlib::MemCpy(gdtBuf + i * BlockSize, TmpBlock, BlockSize);
        }

        for (u32 g = 0; g < GroupCount; g++)
        {
            if (!IsDataBlock(GroupDescs[g].BlockBitmap) || !IsDataBlock(GroupDescs[g].InodeBitmap) ||
                !IsDataBlock(GroupDescs[g].InodeTable))
            {
                Trace(0, "Ext2Fs: group %u descriptor points outside the filesystem", (ulong)g);
                goto fail;
            }
        }
    }

    RootNode = NewVNode(nullptr, "/", VNode::TypeDir, Ext2RootInode, 0);
    if (RootNode == nullptr)
    {
        Trace(0, "Ext2Fs: alloc root vnode failed");
        goto fail;
    }

    if (!ReadOnly && (Super->FeatureRoCompat & ~Ext2RoCompatWritable) != 0)
    {
        Trace(0, "Ext2Fs: ro_compat features 0x%p not maintained by this driver, mounting read-only",
              (ulong)Super->FeatureRoCompat);
        ReadOnly = true;
    }

    if (!ReadOnly)
    {
        /* Like Linux: the valid bit is off while the filesystem is mounted
           for writing, so a crash shows as "not cleanly unmounted" to the
           next mount and to e2fsck. */
        if ((Super->State & Ext2StateValid) == 0)
            Trace(0, "Ext2Fs: %s was not cleanly unmounted", Dev->GetName());

        Super->State = (u16)(Super->State & ~Ext2StateValid);
        Super->MountCount++;
        Super->MountTime = (u32)GetWallTimeSecs();
        if (!WriteSuper())
        {
            Trace(0, "Ext2Fs: cannot write the superblock, mounting read-only");
            ReadOnly = true;
        }
    }

    Mounted = true;
    Trace(0, "Ext2Fs: mounted %s, %u blocks, %u inodes, blocksize %u, %u groups, %s",
          Dev->GetName(), (ulong)Super->BlockCount, (ulong)Super->InodeCount, (ulong)BlockSize,
          (ulong)GroupCount, ReadOnly ? "ro" : "rw");
    return true;

fail:
    if (RootNode != nullptr)
    {
        delete RootNode;
        RootNode = nullptr;
    }
    if (GroupDescs != nullptr)
    {
        Mm::Free(GroupDescs);
        GroupDescs = nullptr;
    }
    if (Super != nullptr)
    {
        delete Super;
        Super = nullptr;
    }
    u8** bufs[] = { &TmpBlock, &DataBuf, &IndBuf, &DindBuf, &BitmapBuf };
    for (ulong i = 0; i < Stdlib::ArraySize(bufs); i++)
    {
        if (*bufs[i] != nullptr)
        {
            Mm::Free(*bufs[i]);
            *bufs[i] = nullptr;
        }
    }
    return false;
}

void Ext2Fs::Unmount()
{
    if (!Mounted)
        return;

    if (!ReadOnly)
    {
        FlushBitmap();
        CommitMeta();
        Super->State = (u16)(Super->State | Ext2StateValid);
        Super->WriteTime = (u32)GetWallTimeSecs();
        WriteSuper();
        Dev->Flush();
    }

    /* Every vnode sits in exactly one Children list, so the tree walk frees
       them all; depth is bounded by the path length that loaded them. */
    if (RootNode != nullptr)
        FreeTree(RootNode);
    RootNode = nullptr;

    if (GroupDescs != nullptr)
    {
        Mm::Free(GroupDescs);
        GroupDescs = nullptr;
    }
    if (Super != nullptr)
    {
        delete Super;
        Super = nullptr;
    }
    u8** bufs[] = { &TmpBlock, &DataBuf, &IndBuf, &DindBuf, &BitmapBuf };
    for (ulong i = 0; i < Stdlib::ArraySize(bufs); i++)
    {
        if (*bufs[i] != nullptr)
        {
            Mm::Free(*bufs[i]);
            *bufs[i] = nullptr;
        }
    }

    IndBufBlock = 0;
    DindBufBlock = 0;
    BitmapBlock = 0;
    BitmapDirty = false;
    MetaDirty = false;
    Mounted = false;
}

bool Ext2Fs::Sync()
{
    if (!Mounted || ReadOnly)
        return true;

    if (!FlushBitmap() || !CommitMeta())
        return false;
    return Dev->Flush();
}

/* --- Inodes --- */

u32 Ext2Fs::InodeGroup(u32 inodeNum)
{
    return (inodeNum - 1) / Super->InodesPerGroup;
}

bool Ext2Fs::ReadInode(u32 inodeNum, Ext2Inode* out)
{
    if (inodeNum == 0 || inodeNum > Super->InodeCount || Super == nullptr || GroupDescs == nullptr)
    {
        Trace(0, "Ext2Fs::ReadInode: inode %u out of range", (ulong)inodeNum);
        return false;
    }

    u32 group = InodeGroup(inodeNum);
    u32 index = (inodeNum - 1) % Super->InodesPerGroup;

    if (group >= GroupCount)
    {
        Trace(0, "Ext2Fs::ReadInode: group %u out of range for inode %u",
              (ulong)group, (ulong)inodeNum);
        return false;
    }

    u32 inodeTableBlock = GroupDescs[group].InodeTable;
    ulong byteOffset = (ulong)index * InodeSize;
    u32 blockInTable = (u32)(byteOffset / BlockSize);
    u32 offsetInBlock = (u32)(byteOffset % BlockSize);

    if (!ReadBlock(inodeTableBlock + blockInTable, TmpBlock))
    {
        Trace(0, "Ext2Fs::ReadInode: read block failed for inode %u", (ulong)inodeNum);
        return false;
    }

    Stdlib::MemCpy(out, TmpBlock + offsetInBlock, sizeof(Ext2Inode));
    return true;
}

/* Read-modify-write of the inode table block, with FUA: an inode commit is
   the point a change becomes real. */
bool Ext2Fs::WriteInode(u32 inodeNum, const Ext2Inode* in)
{
    if (inodeNum == 0 || inodeNum > Super->InodeCount)
    {
        Trace(0, "Ext2Fs::WriteInode: inode %u out of range", (ulong)inodeNum);
        return false;
    }

    u32 group = InodeGroup(inodeNum);
    u32 index = (inodeNum - 1) % Super->InodesPerGroup;
    if (group >= GroupCount)
        return false;

    u32 inodeTableBlock = GroupDescs[group].InodeTable;
    ulong byteOffset = (ulong)index * InodeSize;
    u32 blockInTable = (u32)(byteOffset / BlockSize);
    u32 offsetInBlock = (u32)(byteOffset % BlockSize);

    if (!ReadBlock(inodeTableBlock + blockInTable, TmpBlock))
    {
        Trace(0, "Ext2Fs::WriteInode: read block failed for inode %u", (ulong)inodeNum);
        return false;
    }

    Stdlib::MemCpy(TmpBlock + offsetInBlock, in, sizeof(Ext2Inode));
    if (!WriteBlock(inodeTableBlock + blockInTable, TmpBlock, true))
    {
        Trace(0, "Ext2Fs::WriteInode: write block failed for inode %u", (ulong)inodeNum);
        return false;
    }
    return true;
}

void Ext2Fs::InitInode(Ext2Inode* inode, u16 mode)
{
    u32 now = (u32)GetWallTimeSecs();
    Stdlib::MemSet(inode, 0, sizeof(*inode));
    inode->Mode = mode;
    inode->LinksCount = 1;
    inode->AccessTime = now;
    inode->CreateTime = now;
    inode->ModifyTime = now;
}

/* --- Allocation --- */

bool Ext2Fs::LoadBitmap(u32 blockNum)
{
    if (BitmapBlock == blockNum)
        return true;

    if (!FlushBitmap())
        return false;

    if (!ReadBlock(blockNum, BitmapBuf))
    {
        Trace(0, "Ext2Fs: read bitmap block %u failed", (ulong)blockNum);
        BitmapBlock = 0;
        return false;
    }

    BitmapBlock = blockNum;
    return true;
}

bool Ext2Fs::FlushBitmap()
{
    if (!BitmapDirty)
        return true;

    if (!WriteBlock(BitmapBlock, BitmapBuf, true))
    {
        Trace(0, "Ext2Fs: write bitmap block %u failed", (ulong)BitmapBlock);
        return false;
    }

    BitmapDirty = false;
    return true;
}

/* A free block, from goalGroup if it has one (keeping a file near its
   inode), else from the first group that does. -1 when the disk is full. */
long Ext2Fs::AllocBlock(u32 goalGroup)
{
    for (u32 k = 0; k < GroupCount; k++)
    {
        u32 g = (goalGroup + k) % GroupCount;
        Ext2GroupDesc& gd = GroupDescs[g];
        if (gd.FreeBlockCount == 0)
            continue;

        if (!LoadBitmap(gd.BlockBitmap))
            return -1;

        u32 count = BlocksInGroup(g);
        for (u32 byte = 0; byte < (count + 7) / 8; byte++)
        {
            if (BitmapBuf[byte] == 0xFF)
                continue;

            for (u32 bit = 0; bit < 8; bit++)
            {
                u32 idx = byte * 8 + bit;
                if (idx >= count)
                    break;
                if (BitmapTest(BitmapBuf, idx))
                    continue;

                BitmapSet(BitmapBuf, idx);
                BitmapDirty = true;
                gd.FreeBlockCount--;
                Super->FreeBlockCount--;
                MetaDirty = true;
                return (long)(Super->FirstDataBlock + g * Super->BlocksPerGroup + idx);
            }
        }

        /* The descriptor's count and the bitmap disagree: trust the bitmap
           and stop the descriptor from sending us here again */
        Trace(0, "Ext2Fs: group %u claims %u free blocks but its bitmap is full",
              (ulong)g, (ulong)gd.FreeBlockCount);
        gd.FreeBlockCount = 0;
        MetaDirty = true;
    }

    Trace(0, "Ext2Fs: no free blocks");
    return -1;
}

bool Ext2Fs::FreeBlock(u32 blockNum)
{
    if (!IsDataBlock(blockNum))
    {
        Trace(0, "Ext2Fs: free of block %u outside the filesystem", (ulong)blockNum);
        return false;
    }

    u32 rel = blockNum - Super->FirstDataBlock;
    u32 g = rel / Super->BlocksPerGroup;
    u32 idx = rel % Super->BlocksPerGroup;
    Ext2GroupDesc& gd = GroupDescs[g];

    if (!LoadBitmap(gd.BlockBitmap))
        return false;

    if (!BitmapTest(BitmapBuf, idx))
    {
        Trace(0, "Ext2Fs: block %u already free", (ulong)blockNum);
        return false;
    }

    BitmapClear(BitmapBuf, idx);
    BitmapDirty = true;
    gd.FreeBlockCount++;
    Super->FreeBlockCount++;
    MetaDirty = true;

    /* A cached indirect block that is no longer one */
    if (IndBufBlock == blockNum)
        IndBufBlock = 0;
    if (DindBufBlock == blockNum)
        DindBufBlock = 0;
    return true;
}

long Ext2Fs::AllocInode(u32 goalGroup, bool isDir)
{
    for (u32 k = 0; k < GroupCount; k++)
    {
        u32 g = (goalGroup + k) % GroupCount;
        Ext2GroupDesc& gd = GroupDescs[g];
        if (gd.FreeInodeCount == 0)
            continue;

        if (!LoadBitmap(gd.InodeBitmap))
            return -1;

        u32 count = Super->InodesPerGroup;
        for (u32 byte = 0; byte < (count + 7) / 8; byte++)
        {
            if (BitmapBuf[byte] == 0xFF)
                continue;

            for (u32 bit = 0; bit < 8; bit++)
            {
                u32 idx = byte * 8 + bit;
                if (idx >= count)
                    break;
                if (BitmapTest(BitmapBuf, idx))
                    continue;

                u32 inodeNum = g * Super->InodesPerGroup + idx + 1;
                if (inodeNum < Super->FirstInode || inodeNum > Super->InodeCount)
                    continue;

                BitmapSet(BitmapBuf, idx);
                BitmapDirty = true;
                gd.FreeInodeCount--;
                Super->FreeInodeCount--;
                if (isDir)
                    gd.UsedDirsCount++;
                MetaDirty = true;
                return (long)inodeNum;
            }
        }

        Trace(0, "Ext2Fs: group %u claims %u free inodes but its bitmap is full",
              (ulong)g, (ulong)gd.FreeInodeCount);
        gd.FreeInodeCount = 0;
        MetaDirty = true;
    }

    Trace(0, "Ext2Fs: no free inodes");
    return -1;
}

bool Ext2Fs::FreeInode(u32 inodeNum, bool isDir)
{
    if (inodeNum < Super->FirstInode || inodeNum > Super->InodeCount)
    {
        Trace(0, "Ext2Fs: free of inode %u out of range", (ulong)inodeNum);
        return false;
    }

    u32 g = InodeGroup(inodeNum);
    u32 idx = (inodeNum - 1) % Super->InodesPerGroup;
    Ext2GroupDesc& gd = GroupDescs[g];

    if (!LoadBitmap(gd.InodeBitmap))
        return false;

    if (!BitmapTest(BitmapBuf, idx))
    {
        Trace(0, "Ext2Fs: inode %u already free", (ulong)inodeNum);
        return false;
    }

    BitmapClear(BitmapBuf, idx);
    BitmapDirty = true;
    gd.FreeInodeCount++;
    Super->FreeInodeCount++;
    if (isDir && gd.UsedDirsCount > 0)
        gd.UsedDirsCount--;
    MetaDirty = true;
    return true;
}

/* --- Block mapping --- */

bool Ext2Fs::LoadIndirect(u32 blockNum, u8* buf, u32& cached)
{
    if (cached == blockNum)
        return true;

    if (!ReadBlock(blockNum, buf))
    {
        Trace(0, "Ext2Fs: read indirect block %u failed", (ulong)blockNum);
        cached = 0;
        return false;
    }

    cached = blockNum;
    return true;
}

/* Indirect blocks go down with a plain write; every path that commits an
   inode flushes the device first, so they are on disk before the inode
   that leads to them. */
bool Ext2Fs::WriteIndirect(u32 blockNum, const u8* buf)
{
    if (!WriteBlock(blockNum, buf, false))
    {
        Trace(0, "Ext2Fs: write indirect block %u failed", (ulong)blockNum);
        return false;
    }
    return true;
}

/* Physical block behind logical block logicalBlock of inode. With allocate
   set, a missing data block is allocated (fresh reports that, so the
   caller knows it holds nothing worth reading) and so is a missing
   indirect block on the way, zeroed on disk. Without it, a hole comes back
   as physBlock 0. False means an I/O error or corruption: an on-disk
   pointer that points outside the filesystem, or a file that needs the
   triple-indirect block, which this driver does not do. */
bool Ext2Fs::MapBlock(Ext2Inode* inode, u32 goalGroup, u32 logicalBlock, bool allocate,
                      u32& physBlock, bool& fresh)
{
    physBlock = 0;
    fresh = false;
    u32 blockUnits = BlockSize / Ext2BlocksUnit;

    /* Direct blocks (0..11) */
    if (logicalBlock < Ext2DirectBlocks)
    {
        u32 slot = inode->Block[logicalBlock];
        if (slot != 0)
        {
            if (!IsDataBlock(slot))
            {
                Trace(0, "Ext2Fs: direct block %u points outside the filesystem", (ulong)slot);
                return false;
            }
            physBlock = slot;
            return true;
        }
        if (!allocate)
            return true;

        long b = AllocBlock(goalGroup);
        if (b < 0)
            return false;
        inode->Block[logicalBlock] = (u32)b;
        inode->Blocks += blockUnits;
        physBlock = (u32)b;
        fresh = true;
        return true;
    }

    u32 rel = logicalBlock - Ext2DirectBlocks;
    u32* ptrs;
    u32 indBlock;
    u32 indIndex;

    if (rel < PtrsPerBlock)
    {
        /* Single indirect (12) */
        indBlock = inode->Block[Ext2IndirectBlock];
        indIndex = rel;
        if (indBlock == 0)
        {
            if (!allocate)
                return true;

            long b = AllocBlock(goalGroup);
            if (b < 0)
                return false;
            indBlock = (u32)b;
            Stdlib::MemSet(IndBuf, 0, BlockSize);
            IndBufBlock = indBlock;
            if (!WriteIndirect(indBlock, IndBuf))
                return false;
            inode->Block[Ext2IndirectBlock] = indBlock;
            inode->Blocks += blockUnits;
        }
        else if (!IsDataBlock(indBlock))
        {
            Trace(0, "Ext2Fs: indirect block %u points outside the filesystem", (ulong)indBlock);
            return false;
        }
    }
    else if (rel - PtrsPerBlock < PtrsPerBlock * PtrsPerBlock)
    {
        /* Double indirect (13) */
        rel -= PtrsPerBlock;
        u32 dindBlock = inode->Block[Ext2DIndirectBlock];
        u32 l1Index = rel / PtrsPerBlock;
        indIndex = rel % PtrsPerBlock;

        if (dindBlock == 0)
        {
            if (!allocate)
                return true;

            long b = AllocBlock(goalGroup);
            if (b < 0)
                return false;
            dindBlock = (u32)b;
            Stdlib::MemSet(DindBuf, 0, BlockSize);
            DindBufBlock = dindBlock;
            if (!WriteIndirect(dindBlock, DindBuf))
                return false;
            inode->Block[Ext2DIndirectBlock] = dindBlock;
            inode->Blocks += blockUnits;
        }
        else if (!IsDataBlock(dindBlock))
        {
            Trace(0, "Ext2Fs: double indirect block %u points outside the filesystem", (ulong)dindBlock);
            return false;
        }

        if (!LoadIndirect(dindBlock, DindBuf, DindBufBlock))
            return false;

        u32* l1 = reinterpret_cast<u32*>(DindBuf);
        indBlock = l1[l1Index];
        if (indBlock == 0)
        {
            if (!allocate)
                return true;

            long b = AllocBlock(goalGroup);
            if (b < 0)
                return false;
            indBlock = (u32)b;
            Stdlib::MemSet(IndBuf, 0, BlockSize);
            IndBufBlock = indBlock;
            if (!WriteIndirect(indBlock, IndBuf))
                return false;
            l1[l1Index] = indBlock;
            if (!WriteIndirect(dindBlock, DindBuf))
                return false;
            inode->Blocks += blockUnits;
        }
        else if (!IsDataBlock(indBlock))
        {
            Trace(0, "Ext2Fs: indirect block %u points outside the filesystem", (ulong)indBlock);
            return false;
        }
    }
    else
    {
        /* Failing beats silently returning zeros for the block's data */
        Trace(0, "Ext2Fs: triple indirect not supported (logical block %u)", (ulong)logicalBlock);
        return false;
    }

    if (!LoadIndirect(indBlock, IndBuf, IndBufBlock))
        return false;

    ptrs = reinterpret_cast<u32*>(IndBuf);
    u32 slot = ptrs[indIndex];
    if (slot != 0)
    {
        if (!IsDataBlock(slot))
        {
            Trace(0, "Ext2Fs: block %u points outside the filesystem", (ulong)slot);
            return false;
        }
        physBlock = slot;
        return true;
    }
    if (!allocate)
        return true;

    long b = AllocBlock(goalGroup);
    if (b < 0)
        return false;
    ptrs[indIndex] = (u32)b;
    if (!WriteIndirect(indBlock, IndBuf))
        return false;
    inode->Blocks += blockUnits;
    physBlock = (u32)b;
    fresh = true;
    return true;
}

/* Walk the file's blocks downward from lastKept - 1 to fromBlock, clearing
   each pointer and collecting the block for release, an indirect block too
   once its last entry is gone. Stops when the batch is full; lastKept says
   where to resume. Modified indirect blocks are written before returning,
   so the inode the caller commits next leads to a consistent tree. */
bool Ext2Fs::ReleaseTail(Ext2Inode* inode, u32 fromBlock, u32* batch, ulong& batchCount, u32& lastKept)
{
    u32 blockUnits = BlockSize / Ext2BlocksUnit;
    bool indDirty = false;
    bool dindDirty = false;

    while (lastKept > fromBlock && batchCount < Ext2FreeBatch)
    {
        u32 logical = lastKept - 1;

        if (logical < Ext2DirectBlocks)
        {
            u32 p = inode->Block[logical];
            if (p != 0)
            {
                batch[batchCount++] = p;
                inode->Block[logical] = 0;
                if (inode->Blocks >= blockUnits)
                    inode->Blocks -= blockUnits;
            }
        }
        else if (logical - Ext2DirectBlocks < PtrsPerBlock)
        {
            u32 i = logical - Ext2DirectBlocks;
            u32 indBlock = inode->Block[Ext2IndirectBlock];
            if (indBlock != 0 && IsDataBlock(indBlock))
            {
                if (!LoadIndirect(indBlock, IndBuf, IndBufBlock))
                    return false;
                u32* ptrs = reinterpret_cast<u32*>(IndBuf);
                if (ptrs[i] != 0)
                {
                    batch[batchCount++] = ptrs[i];
                    ptrs[i] = 0;
                    indDirty = true;
                    if (inode->Blocks >= blockUnits)
                        inode->Blocks -= blockUnits;
                }
                if (i == 0)
                {
                    /* Nothing left behind this indirect block */
                    if (batchCount < Ext2FreeBatch)
                    {
                        batch[batchCount++] = indBlock;
                        inode->Block[Ext2IndirectBlock] = 0;
                        if (inode->Blocks >= blockUnits)
                            inode->Blocks -= blockUnits;
                        IndBufBlock = 0;
                        indDirty = false;
                    }
                    else
                    {
                        /* Batch full: keep the empty indirect block for the
                           next round rather than leak it */
                        break;
                    }
                }
            }
            else if (indBlock != 0)
            {
                Trace(0, "Ext2Fs: indirect block %u points outside the filesystem, dropped", (ulong)indBlock);
                inode->Block[Ext2IndirectBlock] = 0;
            }
        }
        else if (logical - Ext2DirectBlocks - PtrsPerBlock < PtrsPerBlock * PtrsPerBlock)
        {
            u32 rel = logical - Ext2DirectBlocks - PtrsPerBlock;
            u32 i1 = rel / PtrsPerBlock;
            u32 i2 = rel % PtrsPerBlock;
            u32 dindBlock = inode->Block[Ext2DIndirectBlock];
            if (dindBlock != 0 && IsDataBlock(dindBlock))
            {
                if (!LoadIndirect(dindBlock, DindBuf, DindBufBlock))
                    return false;
                u32* l1 = reinterpret_cast<u32*>(DindBuf);
                u32 indBlock = l1[i1];
                if (indBlock != 0 && IsDataBlock(indBlock))
                {
                    if (!LoadIndirect(indBlock, IndBuf, IndBufBlock))
                        return false;
                    u32* ptrs = reinterpret_cast<u32*>(IndBuf);
                    if (ptrs[i2] != 0)
                    {
                        batch[batchCount++] = ptrs[i2];
                        ptrs[i2] = 0;
                        indDirty = true;
                        if (inode->Blocks >= blockUnits)
                            inode->Blocks -= blockUnits;
                    }
                    if (i2 == 0)
                    {
                        if (batchCount >= Ext2FreeBatch)
                            break;
                        batch[batchCount++] = indBlock;
                        l1[i1] = 0;
                        dindDirty = true;
                        if (inode->Blocks >= blockUnits)
                            inode->Blocks -= blockUnits;
                        IndBufBlock = 0;
                        indDirty = false;
                    }
                }
                else if (indBlock != 0)
                {
                    Trace(0, "Ext2Fs: indirect block %u points outside the filesystem, dropped", (ulong)indBlock);
                    l1[i1] = 0;
                    dindDirty = true;
                }

                if (rel == 0)
                {
                    if (batchCount >= Ext2FreeBatch)
                        break;
                    batch[batchCount++] = dindBlock;
                    inode->Block[Ext2DIndirectBlock] = 0;
                    if (inode->Blocks >= blockUnits)
                        inode->Blocks -= blockUnits;
                    DindBufBlock = 0;
                    dindDirty = false;
                }
            }
            else if (dindBlock != 0)
            {
                Trace(0, "Ext2Fs: double indirect block %u points outside the filesystem, dropped", (ulong)dindBlock);
                inode->Block[Ext2DIndirectBlock] = 0;
            }
        }
        else
        {
            /* Beyond what this driver maps: nothing to release there */
        }

        lastKept = logical;
    }

    if (indDirty && IndBufBlock != 0 && !WriteIndirect(IndBufBlock, IndBuf))
        return false;
    if (dindDirty && DindBufBlock != 0 && !WriteIndirect(DindBufBlock, DindBuf))
        return false;
    return true;
}

/* --- Data --- */

bool Ext2Fs::ReadInodeData(Ext2Inode* inode, void* buf, ulong len, ulong offset)
{
    u32 fileSize = inode->Size;
    if (offset >= fileSize)
        return false;

    ulong avail = fileSize - offset;
    if (len > avail)
        len = avail;

    u8* dst = static_cast<u8*>(buf);
    ulong bytesRead = 0;
    u32 blockIdx = (u32)(offset / BlockSize);
    u32 byteOff = (u32)(offset % BlockSize);

    while (bytesRead < len)
    {
        u32 physBlock;
        bool fresh;
        if (!MapBlock(inode, 0, blockIdx, false, physBlock, fresh))
        {
            Trace(0, "Ext2Fs::ReadInodeData: block %u unmappable", (ulong)blockIdx);
            return false;
        }

        u32 chunk = BlockSize - byteOff;
        if (chunk > len - bytesRead)
            chunk = (u32)(len - bytesRead);

        if (physBlock == 0)
        {
            /* Sparse block -- fill with zeros */
            Stdlib::MemSet(dst + bytesRead, 0, chunk);
        }
        else
        {
            if (!ReadBlock(physBlock, DataBuf))
            {
                Trace(0, "Ext2Fs::ReadInodeData: read block %u failed", (ulong)physBlock);
                return false;
            }
            Stdlib::MemCpy(dst + bytesRead, DataBuf + byteOff, chunk);
        }

        bytesRead += chunk;
        byteOff = 0;
        blockIdx++;
    }

    return true;
}

/* Write len bytes at offset, allocating what the range needs. Data blocks
   go down with plain writes; the caller flushes the device and then
   commits the inode, so the content is on disk before anything points at
   it. On failure the inode still describes every block allocated so far,
   and the caller commits it as it is, which keeps the bitmap honest. */
bool Ext2Fs::WriteInodeData(Ext2Inode* inode, u32 goalGroup, const void* data, ulong len, ulong offset)
{
    const u8* src = static_cast<const u8*>(data);
    ulong written = 0;
    u32 blockIdx = (u32)(offset / BlockSize);
    u32 byteOff = (u32)(offset % BlockSize);

    while (written < len)
    {
        u32 physBlock;
        bool fresh;
        if (!MapBlock(inode, goalGroup, blockIdx, true, physBlock, fresh))
        {
            Trace(0, "Ext2Fs::WriteInodeData: block %u unmappable", (ulong)blockIdx);
            return false;
        }

        u32 chunk = BlockSize - byteOff;
        if (chunk > len - written)
            chunk = (u32)(len - written);

        if (chunk < BlockSize)
        {
            if (fresh)
                Stdlib::MemSet(DataBuf, 0, BlockSize);
            else if (!ReadBlock(physBlock, DataBuf))
            {
                Trace(0, "Ext2Fs::WriteInodeData: read block %u failed", (ulong)physBlock);
                return false;
            }
        }

        Stdlib::MemCpy(DataBuf + byteOff, src + written, chunk);
        if (!WriteBlock(physBlock, DataBuf, false))
        {
            Trace(0, "Ext2Fs::WriteInodeData: write block %u failed", (ulong)physBlock);
            return false;
        }

        written += chunk;
        byteOff = 0;
        blockIdx++;
    }

    ulong end = offset + len;
    if (end > inode->Size)
        inode->Size = (u32)end;
    inode->ModifyTime = (u32)GetWallTimeSecs();
    return true;
}

/* Shrink (or grow, sparsely) inode to newSize, releasing the blocks past
   it in batches: each batch is cut off the tree, the inode committed
   without it, and only then are its blocks freed in the bitmap, so a crash
   leaks at most one batch to e2fsck and never leaves a block both in use
   and free. */
bool Ext2Fs::TruncateInode(u32 inodeNum, Ext2Inode* inode, ulong newSize)
{
    if (newSize > 0xFFFFFFFFul)
    {
        Trace(0, "Ext2Fs: size %u too large", (ulong)newSize);
        return false;
    }

    inode->ModifyTime = (u32)GetWallTimeSecs();
    if (newSize >= inode->Size)
    {
        /* Growth is a hole: reads see zeros, no block is spent */
        inode->Size = (u32)newSize;
        return true;
    }

    u32 keepBlocks = (u32)((newSize + BlockSize - 1) / BlockSize);
    u32 lastKept = (u32)(((ulong)inode->Size + BlockSize - 1) / BlockSize);
    inode->Size = (u32)newSize;

    u32* batch = static_cast<u32*>(Mm::Alloc(Ext2FreeBatch * sizeof(u32), 0));
    if (batch == nullptr)
    {
        Trace(0, "Ext2Fs: alloc free batch failed");
        return false;
    }

    while (lastKept > keepBlocks)
    {
        ulong batchCount = 0;
        if (!ReleaseTail(inode, keepBlocks, batch, batchCount, lastKept))
        {
            Mm::Free(batch);
            return false;
        }

        if (!Dev->Flush() || !WriteInode(inodeNum, inode))
        {
            Mm::Free(batch);
            return false;
        }

        for (ulong i = 0; i < batchCount; i++)
            FreeBlock(batch[i]);
    }

    Mm::Free(batch);

    /* The kept tail block: zero what lies past the new end, or a later
       write inside that block would leave old bytes in the gap */
    u32 tail = (u32)(newSize % BlockSize);
    if (tail != 0)
    {
        u32 physBlock;
        bool fresh;
        if (!MapBlock(inode, 0, keepBlocks - 1, false, physBlock, fresh))
            return false;
        if (physBlock != 0)
        {
            if (!ReadBlock(physBlock, DataBuf))
                return false;
            Stdlib::MemSet(DataBuf + tail, 0, BlockSize - tail);
            if (!WriteBlock(physBlock, DataBuf, false))
                return false;
        }
    }

    return true;
}

/* --- VNodes --- */

VNode* Ext2Fs::NewVNode(VNode* parent, const char* name, VNode::Type type, u32 inodeNum, ulong size)
{
    VNode* node = new (Mm::NoThrow) VNode();
    if (node == nullptr)
    {
        Trace(0, "Ext2Fs: alloc vnode failed for '%s'", name);
        return nullptr;
    }

    Stdlib::MemSet(node, 0, sizeof(VNode));
    Stdlib::StrnCpy(node->Name, name, sizeof(node->Name));
    node->NodeType = type;
    node->Parent = parent;
    node->Children.Init();
    node->SiblingLink.Init();
    node->Data = nullptr;
    node->Size = (type == VNode::TypeFile) ? size : 0;
    node->Capacity = 0;
    node->Ino = inodeNum;
    node->Flags = 0;
    node->OpenCount = 0;

    if (parent != nullptr)
        parent->Children.InsertTail(&node->SiblingLink);
    return node;
}

void Ext2Fs::FreeTree(VNode* node)
{
    while (!node->Children.IsEmpty())
    {
        Stdlib::ListEntry* entry = node->Children.RemoveHead();
        VNode* child = CONTAINING_RECORD(entry, VNode, SiblingLink);
        child->SiblingLink.Init();
        FreeTree(child);
    }
    delete node;
}

/* --- Directories --- */

/* Read a directory's entries into its vnode the first time it is needed.
   Only what a path walk touches is ever loaded, so a big tree costs memory
   in proportion to what is used, not to what is on disk. */
bool Ext2Fs::LoadDir(VNode* dir)
{
    if (dir == nullptr || dir->NodeType != VNode::TypeDir)
        return false;

    if (dir->Flags & VNode::FlagDirLoaded)
        return true;

    Ext2Inode inode;
    if (!ReadInode((u32)dir->Ino, &inode))
    {
        Trace(0, "Ext2Fs::LoadDir: read inode %u failed", (ulong)dir->Ino);
        return false;
    }

    if ((inode.Mode & Ext2InodeModeTypeMask) != Ext2InodeModeDir)
    {
        Trace(0, "Ext2Fs::LoadDir: inode %u is not a directory", (ulong)dir->Ino);
        return false;
    }

    u32 dirBlocks = (u32)(((ulong)inode.Size + BlockSize - 1) / BlockSize);
    for (u32 blk = 0; blk < dirBlocks; blk++)
    {
        u32 physBlock;
        bool fresh;
        if (!MapBlock(&inode, 0, blk, false, physBlock, fresh))
            return false;
        if (physBlock == 0)
            continue;
        if (!ReadBlock(physBlock, DataBuf))
        {
            Trace(0, "Ext2Fs::LoadDir: read block %u of inode %u failed", (ulong)blk, (ulong)dir->Ino);
            return false;
        }

        ulong pos = 0;
        while (pos + Ext2DirEntryHeader <= BlockSize)
        {
            Ext2DirEntry* de = reinterpret_cast<Ext2DirEntry*>(DataBuf + pos);

            if (de->RecLen < Ext2DirEntryHeader + de->NameLen ||
                de->RecLen > BlockSize - pos || (de->RecLen % Ext2DirEntryAlign) != 0)
            {
                Trace(0, "Ext2Fs::LoadDir: bad dirent at offset %u in inode %u",
                      (ulong)(blk * BlockSize + pos), (ulong)dir->Ino);
                break;
            }

            if (de->Inode != 0 && de->NameLen > 0 && de->Inode <= Super->InodeCount)
            {
                /* Skip "." and ".." */
                bool skip = false;
                if (de->NameLen == 1 && de->Name[0] == '.')
                    skip = true;
                if (de->NameLen == 2 && de->Name[0] == '.' && de->Name[1] == '.')
                    skip = true;

                /* A truncated name would collide with other long names in
                   Lookup; skip the entry instead of silently truncating */
                if (!skip && de->NameLen >= sizeof(VNode::Name))
                {
                    Trace(0, "Ext2Fs::LoadDir: name too long (%u) in inode %u, skipped",
                          (ulong)de->NameLen, (ulong)dir->Ino);
                    skip = true;
                }

                if (!skip)
                {
                    char name[sizeof(VNode::Name)];
                    Stdlib::MemCpy(name, de->Name, de->NameLen);
                    name[de->NameLen] = '\0';

                    /* Everything but files and directories -- symlinks,
                       devices -- is left out of the tree */
                    bool isDir = (de->FileType == Ext2DirTypeDir);
                    bool isFile = (de->FileType == Ext2DirTypeFile);
                    ulong size = 0;
                    if (isFile || de->FileType == Ext2DirTypeUnknown)
                    {
                        Ext2Inode child;
                        if (ReadInode(de->Inode, &child))
                        {
                            u16 type = child.Mode & Ext2InodeModeTypeMask;
                            isDir = (type == Ext2InodeModeDir);
                            isFile = (type == Ext2InodeModeFile);
                            size = child.Size;
                            if (isFile && child.DirAcl != 0)
                            {
                                Trace(0, "Ext2Fs::LoadDir: '%s' is over 4 GiB, skipped", name);
                                isFile = false;
                            }
                        }
                        else
                        {
                            isDir = false;
                            isFile = false;
                        }
                    }

                    /* A directory entry that leads back up the tree is a
                       cycle in the image; following it would never end */
                    if (isDir)
                    {
                        for (VNode* up = dir; up != nullptr; up = up->Parent)
                        {
                            if (up->Ino == de->Inode)
                            {
                                Trace(0, "Ext2Fs::LoadDir: '%s' in inode %u is an ancestor, skipped",
                                      name, (ulong)dir->Ino);
                                isDir = false;
                                break;
                            }
                        }
                    }

                    if (isDir || isFile)
                    {
                        if (NewVNode(dir, name, isDir ? VNode::TypeDir : VNode::TypeFile,
                                     de->Inode, size) == nullptr)
                            return false;
                    }
                }
            }

            pos += de->RecLen;
        }
    }

    dir->Flags |= VNode::FlagDirLoaded;
    return true;
}

/* Put (inodeNum, name) into dir: in the first slack big enough in an
   existing block, else in a new block appended to the directory. dirInode
   is updated in memory (size, mtime, the htree flag dropped); the caller
   commits it. */
bool Ext2Fs::AddDirEntry(VNode* dir, Ext2Inode* dirInode, u32 inodeNum, const char* name, u8 fileType)
{
    ulong nameLen = Stdlib::StrLen(name);
    if (nameLen == 0 || nameLen > Ext2MaxNameLen)
        return false;
    ulong need = DirEntrySize(nameLen);

    dirInode->Flags = dirInode->Flags & ~Ext2InodeFlagIndex;
    dirInode->ModifyTime = (u32)GetWallTimeSecs();

    u32 goalGroup = InodeGroup((u32)dir->Ino);
    u32 dirBlocks = (u32)(((ulong)dirInode->Size + BlockSize - 1) / BlockSize);

    for (u32 blk = 0; blk < dirBlocks; blk++)
    {
        u32 physBlock;
        bool fresh;
        if (!MapBlock(dirInode, goalGroup, blk, false, physBlock, fresh))
            return false;
        if (physBlock == 0)
            continue;
        if (!ReadBlock(physBlock, DataBuf))
            return false;

        ulong pos = 0;
        while (pos + Ext2DirEntryHeader <= BlockSize)
        {
            Ext2DirEntry* de = reinterpret_cast<Ext2DirEntry*>(DataBuf + pos);
            /* An occupied record must hold its own name, or the slack
               computed below would wrap and place the new entry past the
               block */
            if (de->RecLen < Ext2DirEntryHeader || de->RecLen > BlockSize - pos ||
                (de->RecLen % Ext2DirEntryAlign) != 0 ||
                (de->Inode != 0 && de->RecLen < Ext2DirEntryHeader + de->NameLen))
            {
                Trace(0, "Ext2Fs::AddDirEntry: bad dirent at offset %u in inode %u",
                      (ulong)(blk * BlockSize + pos), (ulong)dir->Ino);
                return false;
            }

            ulong used = (de->Inode == 0) ? 0 : DirEntrySize(de->NameLen);
            if (de->RecLen - used >= need)
            {
                Ext2DirEntry* ne;
                if (used == 0)
                {
                    /* A free slot: take it whole */
                    ne = de;
                }
                else
                {
                    u16 total = de->RecLen;
                    de->RecLen = (u16)used;
                    ne = reinterpret_cast<Ext2DirEntry*>(DataBuf + pos + used);
                    ne->RecLen = (u16)(total - used);
                }
                ne->Inode = inodeNum;
                ne->NameLen = (u8)nameLen;
                ne->FileType = fileType;
                Stdlib::MemCpy(ne->Name, name, nameLen);
                return WriteBlock(physBlock, DataBuf, true);
            }

            pos += de->RecLen;
        }
    }

    /* No room: a new block holding this one entry */
    u32 physBlock;
    bool fresh;
    if (!MapBlock(dirInode, goalGroup, dirBlocks, true, physBlock, fresh))
        return false;

    Stdlib::MemSet(DataBuf, 0, BlockSize);
    Ext2DirEntry* ne = reinterpret_cast<Ext2DirEntry*>(DataBuf);
    ne->Inode = inodeNum;
    ne->RecLen = (u16)BlockSize;
    ne->NameLen = (u8)nameLen;
    ne->FileType = fileType;
    Stdlib::MemCpy(ne->Name, name, nameLen);
    if (!WriteBlock(physBlock, DataBuf, true))
        return false;

    dirInode->Size += BlockSize;
    return true;
}

/* Take (inodeNum, name) out of dir: the entry is folded into its
   predecessor's record, or emptied if it leads its block. */
bool Ext2Fs::RemoveDirEntry(VNode* dir, Ext2Inode* dirInode, u32 inodeNum, const char* name)
{
    ulong nameLen = Stdlib::StrLen(name);
    u32 dirBlocks = (u32)(((ulong)dirInode->Size + BlockSize - 1) / BlockSize);

    dirInode->Flags = dirInode->Flags & ~Ext2InodeFlagIndex;
    dirInode->ModifyTime = (u32)GetWallTimeSecs();

    for (u32 blk = 0; blk < dirBlocks; blk++)
    {
        u32 physBlock;
        bool fresh;
        if (!MapBlock(dirInode, 0, blk, false, physBlock, fresh))
            return false;
        if (physBlock == 0)
            continue;
        if (!ReadBlock(physBlock, DataBuf))
            return false;

        ulong pos = 0;
        Ext2DirEntry* prev = nullptr;
        while (pos + Ext2DirEntryHeader <= BlockSize)
        {
            Ext2DirEntry* de = reinterpret_cast<Ext2DirEntry*>(DataBuf + pos);
            if (de->RecLen < Ext2DirEntryHeader + de->NameLen || de->RecLen > BlockSize - pos ||
                (de->RecLen % Ext2DirEntryAlign) != 0)
            {
                Trace(0, "Ext2Fs::RemoveDirEntry: bad dirent at offset %u in inode %u",
                      (ulong)(blk * BlockSize + pos), (ulong)dir->Ino);
                return false;
            }

            if (de->Inode == inodeNum && de->NameLen == nameLen &&
                Stdlib::MemCmp(de->Name, name, nameLen) == 0)
            {
                if (prev != nullptr)
                    prev->RecLen = (u16)(prev->RecLen + de->RecLen);
                else
                    de->Inode = 0;
                return WriteBlock(physBlock, DataBuf, true);
            }

            prev = de;
            pos += de->RecLen;
        }
    }

    Trace(0, "Ext2Fs::RemoveDirEntry: '%s' not in inode %u", name, (ulong)dir->Ino);
    return false;
}

/* Point a moved directory's ".." at its new parent */
bool Ext2Fs::SetDotDot(Ext2Inode* dirInode, u32 parentInodeNum)
{
    u32 physBlock;
    bool fresh;
    if (!MapBlock(dirInode, 0, 0, false, physBlock, fresh) || physBlock == 0)
        return false;
    if (!ReadBlock(physBlock, DataBuf))
        return false;

    Ext2DirEntry* dot = reinterpret_cast<Ext2DirEntry*>(DataBuf);
    if (dot->RecLen < Ext2DirEntryHeader || dot->RecLen + Ext2DirEntryHeader > BlockSize)
        return false;

    Ext2DirEntry* dotdot = reinterpret_cast<Ext2DirEntry*>(DataBuf + dot->RecLen);
    if (dotdot->NameLen != 2 || dotdot->Name[0] != '.' || dotdot->Name[1] != '.')
    {
        Trace(0, "Ext2Fs::SetDotDot: second entry is not ..");
        return false;
    }

    dotdot->Inode = parentInodeNum;
    return WriteBlock(physBlock, DataBuf, true);
}

bool Ext2Fs::LinkCountAdjust(u32 inodeNum, int delta)
{
    Ext2Inode inode;
    if (!ReadInode(inodeNum, &inode))
        return false;

    if (delta < 0 && inode.LinksCount < (u16)(-delta))
        inode.LinksCount = 0;
    else
        inode.LinksCount = (u16)((int)inode.LinksCount + delta);
    inode.ModifyTime = (u32)GetWallTimeSecs();
    return WriteInode(inodeNum, &inode);
}

/* --- FileSystem interface --- */

VNode* Ext2Fs::Lookup(VNode* dir, const char* name)
{
    if (dir == nullptr || name == nullptr)
        return nullptr;

    if (dir->NodeType != VNode::TypeDir)
        return nullptr;

    if (!LoadDir(dir))
        return nullptr;

    Stdlib::ListEntry* head = &dir->Children;
    Stdlib::ListEntry* entry = head->Flink;
    while (entry != head)
    {
        VNode* child = CONTAINING_RECORD(entry, VNode, SiblingLink);
        if (Stdlib::StrCmp(child->Name, name) == 0)
            return child;
        entry = entry->Flink;
    }

    return nullptr;
}

bool Ext2Fs::Read(VNode* file, void* buf, ulong len, ulong offset)
{
    if (file == nullptr || file->NodeType != VNode::TypeFile)
    {
        Trace(0, "Ext2Fs::Read: null file or not a file");
        return false;
    }

    if (len == 0)
        return true;

    u32 inodeNum = (u32)file->Ino;
    Ext2Inode inode;
    if (!ReadInode(inodeNum, &inode))
    {
        Trace(0, "Ext2Fs::Read: read inode %u failed", (ulong)inodeNum);
        return false;
    }

    return ReadInodeData(&inode, buf, len, offset);
}

VNode* Ext2Fs::CreateFile(VNode* dir, const char* name)
{
    if (dir == nullptr || name == nullptr)
    {
        Trace(0, "Ext2Fs::CreateFile: null dir or name");
        return nullptr;
    }

    if (ReadOnly)
    {
        Trace(0, "Ext2Fs::CreateFile: read-only");
        return nullptr;
    }

    if (dir->NodeType != VNode::TypeDir)
    {
        Trace(0, "Ext2Fs::CreateFile: parent is not a dir");
        return nullptr;
    }

    ulong nameLen = Stdlib::StrLen(name);
    if (nameLen == 0 || nameLen >= sizeof(VNode::Name))
    {
        Trace(0, "Ext2Fs::CreateFile: bad name length %u", (ulong)nameLen);
        return nullptr;
    }

    if (Lookup(dir, name) != nullptr)
    {
        Trace(0, "Ext2Fs::CreateFile: '%s' already exists", name);
        return nullptr;
    }

    Ext2Inode dirInode;
    if (!ReadInode((u32)dir->Ino, &dirInode))
        return nullptr;

    long ino = AllocInode(InodeGroup((u32)dir->Ino), false);
    if (ino < 0)
        return nullptr;

    /* The inode and its allocation bit are on disk before anything names
       it: a crash in between leaves an unreferenced inode for e2fsck, not
       a name leading nowhere */
    Ext2Inode inode;
    InitInode(&inode, Ext2InodeModeFileDefault);
    if (!WriteInode((u32)ino, &inode) || !FlushBitmap() || !CommitMeta())
    {
        Trace(0, "Ext2Fs::CreateFile: commit of inode %u failed", (ulong)ino);
        FreeInode((u32)ino, false);
        FlushBitmap();
        CommitMeta();
        return nullptr;
    }

    if (!AddDirEntry(dir, &dirInode, (u32)ino, name, Ext2DirTypeFile))
    {
        Trace(0, "Ext2Fs::CreateFile: add dir entry failed for '%s'", name);
        FreeInode((u32)ino, false);
        FlushBitmap();
        CommitMeta();
        return nullptr;
    }

    if (!FlushBitmap() || !Dev->Flush() || !WriteInode((u32)dir->Ino, &dirInode) || !CommitMeta())
    {
        Trace(0, "Ext2Fs::CreateFile: commit of dir inode %u failed", (ulong)dir->Ino);
        return nullptr;
    }

    return NewVNode(dir, name, VNode::TypeFile, (u32)ino, 0);
}

VNode* Ext2Fs::CreateDir(VNode* dir, const char* name)
{
    if (dir == nullptr || name == nullptr)
    {
        Trace(0, "Ext2Fs::CreateDir: null dir or name");
        return nullptr;
    }

    if (ReadOnly)
    {
        Trace(0, "Ext2Fs::CreateDir: read-only");
        return nullptr;
    }

    if (dir->NodeType != VNode::TypeDir)
    {
        Trace(0, "Ext2Fs::CreateDir: parent is not a dir");
        return nullptr;
    }

    ulong nameLen = Stdlib::StrLen(name);
    if (nameLen == 0 || nameLen >= sizeof(VNode::Name))
    {
        Trace(0, "Ext2Fs::CreateDir: bad name length %u", (ulong)nameLen);
        return nullptr;
    }

    if (Lookup(dir, name) != nullptr)
    {
        Trace(0, "Ext2Fs::CreateDir: '%s' already exists", name);
        return nullptr;
    }

    Ext2Inode dirInode;
    if (!ReadInode((u32)dir->Ino, &dirInode))
        return nullptr;

    long ino = AllocInode(InodeGroup((u32)dir->Ino), true);
    if (ino < 0)
        return nullptr;

    long blk = AllocBlock(InodeGroup((u32)ino));
    if (blk < 0)
    {
        FreeInode((u32)ino, true);
        return nullptr;
    }

    /* "." and "..", the latter spanning the rest of the block */
    Stdlib::MemSet(DataBuf, 0, BlockSize);
    Ext2DirEntry* dot = reinterpret_cast<Ext2DirEntry*>(DataBuf);
    dot->Inode = (u32)ino;
    dot->RecLen = (u16)DirEntrySize(1);
    dot->NameLen = 1;
    dot->FileType = Ext2DirTypeDir;
    dot->Name[0] = '.';
    Ext2DirEntry* dotdot = reinterpret_cast<Ext2DirEntry*>(DataBuf + dot->RecLen);
    dotdot->Inode = (u32)dir->Ino;
    dotdot->RecLen = (u16)(BlockSize - dot->RecLen);
    dotdot->NameLen = 2;
    dotdot->FileType = Ext2DirTypeDir;
    dotdot->Name[0] = '.';
    dotdot->Name[1] = '.';

    Ext2Inode inode;
    InitInode(&inode, Ext2InodeModeDirDefault);
    inode.LinksCount = 2;
    inode.Size = BlockSize;
    inode.Blocks = BlockSize / Ext2BlocksUnit;
    inode.Block[0] = (u32)blk;

    if (!WriteBlock((u32)blk, DataBuf, true) || !WriteInode((u32)ino, &inode) ||
        !FlushBitmap() || !CommitMeta())
    {
        Trace(0, "Ext2Fs::CreateDir: commit of inode %u failed", (ulong)ino);
        FreeBlock((u32)blk);
        FreeInode((u32)ino, true);
        FlushBitmap();
        CommitMeta();
        return nullptr;
    }

    if (!AddDirEntry(dir, &dirInode, (u32)ino, name, Ext2DirTypeDir))
    {
        Trace(0, "Ext2Fs::CreateDir: add dir entry failed for '%s'", name);
        FreeBlock((u32)blk);
        FreeInode((u32)ino, true);
        FlushBitmap();
        CommitMeta();
        return nullptr;
    }

    /* The new directory's ".." is a link to the parent */
    dirInode.LinksCount++;
    if (!FlushBitmap() || !Dev->Flush() || !WriteInode((u32)dir->Ino, &dirInode) || !CommitMeta())
    {
        Trace(0, "Ext2Fs::CreateDir: commit of dir inode %u failed", (ulong)dir->Ino);
        return nullptr;
    }

    VNode* node = NewVNode(dir, name, VNode::TypeDir, (u32)ino, 0);
    if (node != nullptr)
        node->Flags |= VNode::FlagDirLoaded;
    return node;
}

bool Ext2Fs::Write(VNode* file, const void* data, ulong len, ulong offset)
{
    if (file == nullptr || file->NodeType != VNode::TypeFile)
    {
        Trace(0, "Ext2Fs::Write: null file or not a file");
        return false;
    }

    if (ReadOnly)
    {
        Trace(0, "Ext2Fs::Write: read-only");
        return false;
    }

    if (len == 0)
        return true;

    ulong end = offset + len;
    if (end < offset || end > 0xFFFFFFFFul)
    {
        Trace(0, "Ext2Fs::Write: offset %u + len %u too large", (ulong)offset, (ulong)len);
        return false;
    }

    u32 inodeNum = (u32)file->Ino;
    Ext2Inode inode;
    if (!ReadInode(inodeNum, &inode))
    {
        Trace(0, "Ext2Fs::Write: read inode %u failed", (ulong)inodeNum);
        return false;
    }

    if ((inode.Mode & Ext2InodeModeTypeMask) != Ext2InodeModeFile)
    {
        Trace(0, "Ext2Fs::Write: inode %u is not a regular file", (ulong)inodeNum);
        return false;
    }

    bool ok = WriteInodeData(&inode, InodeGroup(inodeNum), data, len, offset);

    /* Commit order: the allocation bits, the data and indirect blocks, then
       the inode that leads to them, then the counts. Also on failure: the
       blocks the inode picked up are then owned rather than leaked. */
    if (!FlushBitmap() || !Dev->Flush() || !WriteInode(inodeNum, &inode) || !CommitMeta())
    {
        Trace(0, "Ext2Fs::Write: commit of inode %u failed", (ulong)inodeNum);
        return false;
    }

    file->Size = inode.Size;
    return ok;
}

bool Ext2Fs::Truncate(VNode* file, ulong size)
{
    if (file == nullptr || file->NodeType != VNode::TypeFile)
    {
        Trace(0, "Ext2Fs::Truncate: null file or not a file");
        return false;
    }

    if (ReadOnly)
    {
        Trace(0, "Ext2Fs::Truncate: read-only");
        return false;
    }

    u32 inodeNum = (u32)file->Ino;
    Ext2Inode inode;
    if (!ReadInode(inodeNum, &inode))
    {
        Trace(0, "Ext2Fs::Truncate: read inode %u failed", (ulong)inodeNum);
        return false;
    }

    if ((inode.Mode & Ext2InodeModeTypeMask) != Ext2InodeModeFile)
    {
        Trace(0, "Ext2Fs::Truncate: inode %u is not a regular file", (ulong)inodeNum);
        return false;
    }

    bool ok = TruncateInode(inodeNum, &inode, size);

    if (!Dev->Flush() || !WriteInode(inodeNum, &inode) || !FlushBitmap() || !CommitMeta())
    {
        Trace(0, "Ext2Fs::Truncate: commit of inode %u failed", (ulong)inodeNum);
        return false;
    }

    file->Size = inode.Size;
    return ok;
}

bool Ext2Fs::Rename(VNode* node, VNode* newDir, const char* newName)
{
    if (node == nullptr || newDir == nullptr || newName == nullptr)
    {
        Trace(0, "Ext2Fs::Rename: null node, dir or name");
        return false;
    }

    if (ReadOnly)
    {
        Trace(0, "Ext2Fs::Rename: read-only");
        return false;
    }

    if (node->Parent == nullptr)
    {
        Trace(0, "Ext2Fs::Rename: cannot rename root");
        return false;
    }

    if (newDir->NodeType != VNode::TypeDir)
    {
        Trace(0, "Ext2Fs::Rename: target parent is not a dir");
        return false;
    }

    ulong nameLen = Stdlib::StrLen(newName);
    if (nameLen == 0 || nameLen >= sizeof(VNode::Name))
    {
        Trace(0, "Ext2Fs::Rename: bad name length %u", (ulong)nameLen);
        return false;
    }

    if (Lookup(newDir, newName) != nullptr)
    {
        Trace(0, "Ext2Fs::Rename: '%s' already exists", newName);
        return false;
    }

    VNode* oldDir = node->Parent;
    bool isDir = (node->NodeType == VNode::TypeDir);
    bool moved = (oldDir != newDir);
    u32 inodeNum = (u32)node->Ino;

    Ext2Inode newDirInode;
    if (!ReadInode((u32)newDir->Ino, &newDirInode))
        return false;

    /* The new name first, the old one second: a crash between the two
       leaves the file reachable under both, which e2fsck reduces to one,
       rather than under neither */
    if (!AddDirEntry(newDir, &newDirInode, inodeNum, newName, isDir ? Ext2DirTypeDir : Ext2DirTypeFile))
    {
        Trace(0, "Ext2Fs::Rename: add dir entry failed for '%s'", newName);
        return false;
    }

    if (moved)
    {
        if (!FlushBitmap() || !Dev->Flush() || !WriteInode((u32)newDir->Ino, &newDirInode))
            return false;

        Ext2Inode oldDirInode;
        if (!ReadInode((u32)oldDir->Ino, &oldDirInode))
            return false;
        if (!RemoveDirEntry(oldDir, &oldDirInode, inodeNum, node->Name))
            return false;

        if (isDir)
        {
            /* The moved directory's ".." now links the new parent */
            Ext2Inode inode;
            if (!ReadInode(inodeNum, &inode) || !SetDotDot(&inode, (u32)newDir->Ino))
                return false;
            oldDirInode.LinksCount = (oldDirInode.LinksCount > 0) ? (u16)(oldDirInode.LinksCount - 1) : 0;
            if (!LinkCountAdjust((u32)newDir->Ino, 1))
                return false;
        }

        if (!WriteInode((u32)oldDir->Ino, &oldDirInode) || !CommitMeta())
            return false;
    }
    else
    {
        if (!RemoveDirEntry(oldDir, &newDirInode, inodeNum, node->Name))
            return false;
        if (!FlushBitmap() || !Dev->Flush() || !WriteInode((u32)newDir->Ino, &newDirInode) || !CommitMeta())
            return false;
    }

    node->SiblingLink.RemoveInit();
    Stdlib::StrnCpy(node->Name, newName, sizeof(node->Name));
    node->Parent = newDir;
    newDir->Children.InsertTail(&node->SiblingLink);
    return true;
}

/* Take node out of its parent, release its blocks and inode, and free the
   vnode; a directory goes with everything under it. The name goes first,
   so a crash leaves at worst an orphan for e2fsck. */
bool Ext2Fs::RemoveNode(VNode* node, u32 depth)
{
    if (depth >= Ext2MaxDirDepth)
    {
        Trace(0, "Ext2Fs::Remove: dir depth limit %u hit", (ulong)Ext2MaxDirDepth);
        return false;
    }

    bool isDir = (node->NodeType == VNode::TypeDir);
    if (isDir)
    {
        if (!LoadDir(node))
            return false;
        while (!node->Children.IsEmpty())
        {
            VNode* child = CONTAINING_RECORD(node->Children.Flink, VNode, SiblingLink);
            if (!RemoveNode(child, depth + 1))
                return false;
        }
    }

    VNode* parent = node->Parent;
    u32 inodeNum = (u32)node->Ino;

    Ext2Inode parentInode;
    if (!ReadInode((u32)parent->Ino, &parentInode))
        return false;
    if (!RemoveDirEntry(parent, &parentInode, inodeNum, node->Name))
        return false;
    if (isDir)
        parentInode.LinksCount = (parentInode.LinksCount > 0) ? (u16)(parentInode.LinksCount - 1) : 0;
    if (!WriteInode((u32)parent->Ino, &parentInode))
        return false;

    Ext2Inode inode;
    if (!ReadInode(inodeNum, &inode))
        return false;
    if (!TruncateInode(inodeNum, &inode, 0))
        return false;
    inode.LinksCount = 0;
    inode.DeleteTime = (u32)GetWallTimeSecs();
    if (!Dev->Flush() || !WriteInode(inodeNum, &inode))
        return false;
    FreeInode(inodeNum, isDir);

    node->SiblingLink.RemoveInit();
    delete node;
    return true;
}

bool Ext2Fs::Remove(VNode* node)
{
    if (node == nullptr)
    {
        Trace(0, "Ext2Fs::Remove: null node");
        return false;
    }

    if (ReadOnly)
    {
        Trace(0, "Ext2Fs::Remove: read-only");
        return false;
    }

    if (node->Parent == nullptr)
    {
        Trace(0, "Ext2Fs::Remove: cannot remove root");
        return false;
    }

    bool ok = RemoveNode(node, 0);
    if (!FlushBitmap() || !CommitMeta())
        return false;
    return ok;
}

}
