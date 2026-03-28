#include "ext2.h"

#include <lib/stdlib.h>
#include <mm/new.h>
#include <include/const.h>
#include <kernel/trace.h>

namespace Kernel
{

Ext2Fs::Ext2Fs(BlockDevice* dev)
    : Dev(dev)
    , Super(nullptr)
    , GroupDescs(nullptr)
    , BlockSize(0)
    , GroupCount(0)
    , InodeSize(128)
    , RootNode(nullptr)
    , VNodeCount(0)
    , Mounted(false)
    , TmpBlock(nullptr)
{
    Stdlib::MemSet(VNodes, 0, sizeof(VNodes));
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

    if (Dev != nullptr)
        Stdlib::SnPrintf(buf, bufSize, "%s", Dev->GetName());
    else
        buf[0] = '\0';
}

bool Ext2Fs::ReadBlock(u32 blockNum, void* buf)
{
    if (Dev == nullptr)
        return false;

    u32 sectorsPerBlock = BlockSize / (u32)Dev->GetSectorSize();
    u64 startSector = (u64)blockNum * sectorsPerBlock;
    return Dev->ReadSectors(startSector, buf, sectorsPerBlock);
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
    if (TmpBlock == nullptr)
    {
        Trace(0, "Ext2Fs: alloc tmp block failed");
        return false;
    }

    /* Read superblock: it starts at byte offset 1024 from partition start.
       For a 512-byte sector device, that is sector 2 (2 sectors). */
    u32 sbSectorStart = Ext2SuperBlockOffset / (u32)Dev->GetSectorSize();
    u32 sbSectorCount = sizeof(Ext2SuperBlock) / (u32)Dev->GetSectorSize();
    if (sbSectorCount == 0)
        sbSectorCount = 1;

    /* Use TmpBlock as scratch; copy superblock out of it */
    Stdlib::MemSet(TmpBlock, 0, Const::PageSize);
    if (!Dev->ReadSectors(sbSectorStart, TmpBlock, sbSectorCount))
    {
        Trace(0, "Ext2Fs: failed to read superblock");
        goto fail;
    }

    Super = new Ext2SuperBlock();
    if (Super == nullptr)
    {
        Trace(0, "Ext2Fs: alloc superblock failed");
        goto fail;
    }
    Stdlib::MemCpy(Super, TmpBlock, sizeof(Ext2SuperBlock));

    if (Super->Magic != Ext2Magic)
    {
        Trace(0, "Ext2Fs: bad magic 0x%p", (ulong)Super->Magic);
        goto fail;
    }

    BlockSize = 1024u << Super->LogBlockSize;
    if (BlockSize < 1024 || BlockSize > Const::PageSize)
    {
        Trace(0, "Ext2Fs: unsupported block size %u", (ulong)BlockSize);
        goto fail;
    }

    InodeSize = 128;
    if (Super->RevLevel >= 1 && Super->InodeSize > 0)
        InodeSize = Super->InodeSize;

    GroupCount = (Super->BlockCount + Super->BlocksPerGroup - 1) / Super->BlocksPerGroup;
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
        u32 gdtBlock = Super->FirstDataBlock + 1;
        ulong gdtSize = (ulong)GroupCount * sizeof(Ext2GroupDesc);
        ulong gdtBlocks = (gdtSize + BlockSize - 1) / BlockSize;

        GroupDescs = static_cast<Ext2GroupDesc*>(Mm::Alloc(gdtBlocks * BlockSize, 0));
        if (GroupDescs == nullptr)
        {
            Trace(0, "Ext2Fs: alloc group descs failed");
            goto fail;
        }

        u8* gdtBuf = reinterpret_cast<u8*>(GroupDescs);
        for (ulong i = 0; i < gdtBlocks; i++)
        {
            if (!ReadBlock(gdtBlock + (u32)i, TmpBlock))
            {
                Trace(0, "Ext2Fs: failed to read group desc block %u", (ulong)(gdtBlock + i));
                goto fail;
            }
            ulong copyLen = BlockSize;
            if (copyLen > gdtSize - i * BlockSize)
                copyLen = gdtSize - i * BlockSize;
            Stdlib::MemCpy(gdtBuf + i * BlockSize, TmpBlock, copyLen);
        }
    }

    /* Load root directory (inode 2) */
    RootNode = LoadDir(Ext2RootInode);
    if (RootNode == nullptr)
    {
        Trace(0, "Ext2Fs: failed to load root directory");
        goto fail;
    }

    Mounted = true;
    Trace(0, "Ext2Fs: mounted %s, %u blocks, %u inodes, blocksize %u",
          Dev->GetName(), (ulong)Super->BlockCount, (ulong)Super->InodeCount, (ulong)BlockSize);
    return true;

fail:
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
    if (TmpBlock != nullptr)
    {
        Mm::Free(TmpBlock);
        TmpBlock = nullptr;
    }
    return false;
}

void Ext2Fs::Unmount()
{
    if (!Mounted)
        return;

    for (ulong i = 0; i < VNodeCount; i++)
    {
        if (VNodes[i].Node != nullptr)
        {
            delete VNodes[i].Node;
            VNodes[i].Node = nullptr;
        }
    }
    VNodeCount = 0;
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
    if (TmpBlock != nullptr)
    {
        Mm::Free(TmpBlock);
        TmpBlock = nullptr;
    }

    Mounted = false;
}

VNode* Ext2Fs::GetRoot()
{
    return RootNode;
}

BlockDevice* Ext2Fs::GetDevice()
{
    return Dev;
}

/* --- Inode I/O --- */

bool Ext2Fs::ReadInode(u32 inodeNum, Ext2Inode* out)
{
    if (inodeNum == 0 || Super == nullptr || GroupDescs == nullptr)
        return false;

    u32 group = (inodeNum - 1) / Super->InodesPerGroup;
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

/* --- Block mapping --- */

u32 Ext2Fs::GetBlockNum(Ext2Inode* inode, u32 logicalBlock)
{
    u32 ptrsPerBlock = BlockSize / sizeof(u32);

    /* Direct blocks (0..11) */
    if (logicalBlock < Ext2DirectBlocks)
        return inode->Block[logicalBlock];

    logicalBlock -= Ext2DirectBlocks;

    /* Single indirect (12) */
    if (logicalBlock < ptrsPerBlock)
    {
        u32 indBlock = inode->Block[Ext2IndirectBlock];
        if (indBlock == 0)
            return 0;

        if (!ReadBlock(indBlock, TmpBlock))
            return 0;

        u32* ptrs = reinterpret_cast<u32*>(TmpBlock);
        return ptrs[logicalBlock];
    }

    logicalBlock -= ptrsPerBlock;

    /* Double indirect (13) */
    if (logicalBlock < ptrsPerBlock * ptrsPerBlock)
    {
        u32 dindBlock = inode->Block[Ext2DIndirectBlock];
        if (dindBlock == 0)
            return 0;

        if (!ReadBlock(dindBlock, TmpBlock))
            return 0;

        u32* l1 = reinterpret_cast<u32*>(TmpBlock);
        u32 l1Index = logicalBlock / ptrsPerBlock;
        u32 l2Index = logicalBlock % ptrsPerBlock;

        u32 indBlock = l1[l1Index];
        if (indBlock == 0)
            return 0;

        if (!ReadBlock(indBlock, TmpBlock))
            return 0;

        u32* l2 = reinterpret_cast<u32*>(TmpBlock);
        return l2[l2Index];
    }

    Trace(0, "Ext2Fs::GetBlockNum: triple indirect not supported");
    return 0;
}

/* --- Data read --- */

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

    /* Allocate a separate read buffer so we don't clobber TmpBlock
       (GetBlockNum uses TmpBlock for indirect blocks) */
    u8* readBuf = static_cast<u8*>(Mm::Alloc(Const::PageSize, 0));
    if (readBuf == nullptr)
    {
        Trace(0, "Ext2Fs::ReadInodeData: alloc read buf failed");
        return false;
    }

    while (bytesRead < len)
    {
        u32 physBlock = GetBlockNum(inode, blockIdx);
        if (physBlock == 0)
        {
            /* Sparse block — fill with zeros */
            u32 chunk = BlockSize - byteOff;
            if (chunk > len - bytesRead)
                chunk = (u32)(len - bytesRead);
            Stdlib::MemSet(dst + bytesRead, 0, chunk);
            bytesRead += chunk;
            byteOff = 0;
            blockIdx++;
            continue;
        }

        if (!ReadBlock(physBlock, readBuf))
        {
            Trace(0, "Ext2Fs::ReadInodeData: read block %u failed", (ulong)physBlock);
            Mm::Free(readBuf);
            return false;
        }

        u32 chunk = BlockSize - byteOff;
        if (chunk > len - bytesRead)
            chunk = (u32)(len - bytesRead);

        Stdlib::MemCpy(dst + bytesRead, readBuf + byteOff, chunk);
        bytesRead += chunk;
        byteOff = 0;
        blockIdx++;
    }

    Mm::Free(readBuf);
    return true;
}

/* --- VNode cache --- */

VNode* Ext2Fs::FindVNode(u32 inodeNum)
{
    for (ulong i = 0; i < VNodeCount; i++)
    {
        if (VNodes[i].InodeNum == inodeNum)
            return VNodes[i].Node;
    }
    return nullptr;
}

void Ext2Fs::FreeVNode(VNode* vnode)
{
    if (vnode == nullptr)
        return;

    for (ulong i = 0; i < VNodeCount; i++)
    {
        if (VNodes[i].Node == vnode)
        {
            VNodes[i] = VNodes[VNodeCount - 1];
            VNodeCount--;
            break;
        }
    }

    vnode->SiblingLink.RemoveInit();
    delete vnode;
}

/* --- Directory loading --- */

VNode* Ext2Fs::LoadDir(u32 inodeNum)
{
    VNode* existing = FindVNode(inodeNum);
    if (existing != nullptr)
        return existing;

    Ext2Inode inode;
    if (!ReadInode(inodeNum, &inode))
    {
        Trace(0, "Ext2Fs::LoadDir: read inode %u failed", (ulong)inodeNum);
        return nullptr;
    }

    if ((inode.Mode & Ext2InodeModeDir) == 0)
    {
        Trace(0, "Ext2Fs::LoadDir: inode %u is not a directory", (ulong)inodeNum);
        return nullptr;
    }

    VNode* dirNode = new VNode();
    if (dirNode == nullptr)
    {
        Trace(0, "Ext2Fs::LoadDir: alloc vnode failed for inode %u", (ulong)inodeNum);
        return nullptr;
    }

    Stdlib::MemSet(dirNode, 0, sizeof(VNode));
    Stdlib::StrnCpy(dirNode->Name, (inodeNum == Ext2RootInode) ? "/" : "?", sizeof(dirNode->Name));
    dirNode->NodeType = VNode::TypeDir;
    dirNode->Parent = nullptr;
    dirNode->Children.Init();
    dirNode->SiblingLink.Init();
    dirNode->Data = nullptr;
    dirNode->Size = 0;
    dirNode->Capacity = inodeNum;

    if (VNodeCount >= MaxCachedVNodes)
    {
        Trace(0, "Ext2Fs::LoadDir: VNode cache full");
        delete dirNode;
        return nullptr;
    }

    VNodes[VNodeCount].InodeNum = inodeNum;
    VNodes[VNodeCount].Node = dirNode;
    VNodeCount++;

    /* Read directory data and parse entries */
    u32 dirSize = inode.Size;
    u8* dirBuf = static_cast<u8*>(Mm::Alloc(dirSize < Const::PageSize ? Const::PageSize : dirSize, 0));
    if (dirBuf == nullptr)
    {
        Trace(0, "Ext2Fs::LoadDir: alloc dir buf failed for inode %u", (ulong)inodeNum);
        return dirNode;
    }

    if (!ReadInodeData(&inode, dirBuf, dirSize, 0))
    {
        Trace(0, "Ext2Fs::LoadDir: read dir data failed for inode %u", (ulong)inodeNum);
        Mm::Free(dirBuf);
        return dirNode;
    }

    u32 pos = 0;
    while (pos + sizeof(Ext2DirEntry) <= dirSize)
    {
        Ext2DirEntry* de = reinterpret_cast<Ext2DirEntry*>(dirBuf + pos);

        if (de->RecLen == 0)
            break;

        if (de->Inode != 0 && de->NameLen > 0)
        {
            /* Skip "." and ".." */
            bool skip = false;
            if (de->NameLen == 1 && de->Name[0] == '.')
                skip = true;
            if (de->NameLen == 2 && de->Name[0] == '.' && de->Name[1] == '.')
                skip = true;

            if (!skip)
            {
                bool isDir = (de->FileType == Ext2DirTypeDir);

                VNode* child;
                if (isDir)
                {
                    child = LoadDir(de->Inode);
                }
                else
                {
                    child = FindVNode(de->Inode);
                    if (child == nullptr)
                    {
                        child = new VNode();
                        if (child != nullptr)
                        {
                            Stdlib::MemSet(child, 0, sizeof(VNode));
                            child->NodeType = VNode::TypeFile;
                            child->Parent = nullptr;
                            child->Children.Init();
                            child->SiblingLink.Init();
                            child->Data = nullptr;
                            child->Capacity = de->Inode;

                            /* Read file inode to get size */
                            Ext2Inode fileInode;
                            if (ReadInode(de->Inode, &fileInode))
                                child->Size = fileInode.Size;
                            else
                                child->Size = 0;

                            if (VNodeCount < MaxCachedVNodes)
                            {
                                VNodes[VNodeCount].InodeNum = de->Inode;
                                VNodes[VNodeCount].Node = child;
                                VNodeCount++;
                            }
                        }
                    }
                }

                if (child != nullptr)
                {
                    ulong nameLen = de->NameLen;
                    if (nameLen >= sizeof(child->Name))
                        nameLen = sizeof(child->Name) - 1;
                    Stdlib::MemCpy(child->Name, de->Name, nameLen);
                    child->Name[nameLen] = '\0';

                    child->Parent = dirNode;
                    if (child->SiblingLink.IsEmpty())
                        dirNode->Children.InsertTail(&child->SiblingLink);
                }
            }
        }

        pos += de->RecLen;
    }

    Mm::Free(dirBuf);
    return dirNode;
}

/* --- FileSystem interface --- */

VNode* Ext2Fs::Lookup(VNode* dir, const char* name)
{
    if (dir == nullptr || name == nullptr)
        return nullptr;

    if (dir->NodeType != VNode::TypeDir)
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

    u32 inodeNum = (u32)file->Capacity;
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
    (void)dir;
    (void)name;
    return nullptr;
}

VNode* Ext2Fs::CreateDir(VNode* dir, const char* name)
{
    (void)dir;
    (void)name;
    return nullptr;
}

bool Ext2Fs::Write(VNode* file, const void* data, ulong len)
{
    (void)file;
    (void)data;
    (void)len;
    return false;
}

bool Ext2Fs::Remove(VNode* node)
{
    (void)node;
    return false;
}

}
