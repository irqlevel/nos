#include "nanofs.h"

#include <lib/stdlib.h>
#include <lib/bitmap.h>
#include <lib/checksum.h>
#include <mm/new.h>
#include <kernel/trace.h>
#include <kernel/asm.h>
#include <drivers/pit.h>

namespace Kernel
{

NanoFs::NanoFs(BlockDevice* dev)
    : Io(dev, NanoBlockSize)
{
    Stdlib::MemSet(&Super, 0, sizeof(Super));
    Stdlib::MemSet(VNodes, 0, sizeof(VNodes));
}

NanoFs::~NanoFs()
{
    Unmount();
}

void NanoFs::Unmount()
{
    for (u32 i = 0; i < NanoInodeCount; i++)
    {
        if (VNodes[i] != nullptr)
        {
            delete VNodes[i];
            VNodes[i] = nullptr;
        }
    }
}

bool NanoFs::Mount()
{
    if (!Io.ReadBlock(0, &Super))
    {
        Trace(0, "NanoFs: failed to read superblock");
        return false;
    }

    if (Super.Magic != NanoMagic)
    {
        Trace(0, "NanoFs: bad magic 0x%p", (ulong)Super.Magic);
        return false;
    }

    if (Super.Version != NanoVersion)
    {
        Trace(0, "NanoFs: unsupported version %u", (ulong)Super.Version);
        return false;
    }

    if (!VerifySuperChecksum())
    {
        Trace(0, "NanoFs: superblock checksum mismatch");
        return false;
    }

    // Load root VNode (inode 0)
    if (LoadVNode(0) == nullptr)
    {
        Trace(0, "NanoFs: failed to load root inode");
        return false;
    }

    Trace(0, "NanoFs: mounted, %u inodes, %u data blocks",
          (ulong)Super.InodeCount, (ulong)Super.DataBlockCount);
    return true;
}

bool NanoFs::Format(BlockDevice* dev)
{
    BlockIo io(dev, NanoBlockSize);

    NanoSuperBlock* super = new NanoSuperBlock();
    if (super == nullptr)
    {
        Trace(0, "NanoFs::Format: alloc superblock failed");
        return false;
    }

    Stdlib::MemSet(super, 0, sizeof(*super));

    super->Magic = NanoMagic;
    super->Version = NanoVersion;
    super->BlockSize = NanoBlockSize;
    super->InodeCount = NanoInodeCount;
    super->DataBlockCount = NanoDataBlockCount;
    super->InodeStartBlock = NanoInodeStart;
    super->DataStartBlock = NanoDataStart;

    // Generate UUID from TSC + uptime
    u64 tsc = ReadTsc();
    u64 uptime = Pit::GetInstance().GetTime().NanoSecs;
    u8 seed[16];
    Stdlib::MemCpy(&seed[0], &tsc, 8);
    Stdlib::MemCpy(&seed[8], &uptime, 8);
    // Hash the seed into UUID using CRC32
    u32 h1 = Stdlib::Crc32(seed, 16);
    u32 h2 = Stdlib::Crc32(&h1, 4);
    u32 h3 = Stdlib::Crc32(&h2, 4);
    u32 h4 = Stdlib::Crc32(&h3, 4);
    Stdlib::MemCpy(&super->Uuid[0], &h1, 4);
    Stdlib::MemCpy(&super->Uuid[4], &h2, 4);
    Stdlib::MemCpy(&super->Uuid[8], &h3, 4);
    Stdlib::MemCpy(&super->Uuid[12], &h4, 4);

    // Mark inode 0 as used (root dir)
    Stdlib::Bitmap inodeBm(super->InodeBitmap, NanoInodeCount);
    inodeBm.SetBit(0);

    // Allocate one data block for root directory entries
    Stdlib::Bitmap dataBm(super->DataBitmap, NanoDataBlockCount);
    dataBm.SetBit(0);

    // Compute superblock checksum
    super->Checksum = 0;
    super->Checksum = Stdlib::Crc32(super, sizeof(*super));

    bool ok = io.WriteBlock(0, super);
    delete super;

    if (!ok)
    {
        Trace(0, "NanoFs::Format: failed to write superblock");
        return false;
    }

    // Write root inode (inode 0)
    NanoInode* rootInode = new NanoInode();
    if (rootInode == nullptr)
    {
        Trace(0, "NanoFs::Format: alloc root inode failed");
        return false;
    }

    Stdlib::MemSet(rootInode, 0, sizeof(*rootInode));
    rootInode->Type = NanoInodeTypeDir;
    rootInode->Size = 0;
    Stdlib::StrnCpy(rootInode->Name, "/", sizeof(rootInode->Name));
    rootInode->ParentInode = 0;
    rootInode->Blocks[0] = 0; // first data block
    rootInode->DataChecksum = 0;

    rootInode->Checksum = 0;
    rootInode->Checksum = Stdlib::Crc32(rootInode, sizeof(*rootInode));

    ok = io.WriteBlock(NanoInodeStart, rootInode);
    delete rootInode;

    if (!ok)
    {
        Trace(0, "NanoFs::Format: failed to write root inode");
        return false;
    }

    // Zero out the root directory data block - reuse inode buffer space
    u8* zeroBuf = (u8*)Mm::Alloc(NanoBlockSize, 0);
    if (zeroBuf == nullptr)
    {
        Trace(0, "NanoFs::Format: alloc zero buf failed");
        return false;
    }

    Stdlib::MemSet(zeroBuf, 0, NanoBlockSize);
    ok = io.WriteBlock(NanoDataStart, zeroBuf);
    Mm::Free(zeroBuf);

    if (!ok)
    {
        Trace(0, "NanoFs::Format: failed to write root dir data");
        return false;
    }

    if (!io.Flush())
    {
        Trace(0, "NanoFs::Format: flush failed");
        return false;
    }

    Trace(0, "NanoFs::Format: done");
    return true;
}

const char* NanoFs::GetName()
{
    return "nanofs";
}

void NanoFs::GetInfo(char* buf, ulong bufSize)
{
    if (buf == nullptr || bufSize < 38)
    {
        if (buf && bufSize)
            buf[0] = '\0';
        return;
    }

    // Format: "uuid=XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX"
    const char* hex = "0123456789abcdef";
    ulong pos = 0;
    const char* prefix = "uuid=";
    for (ulong i = 0; prefix[i] && pos < bufSize - 1; i++)
        buf[pos++] = prefix[i];
    for (ulong i = 0; i < 16 && pos + 1 < bufSize; i++)
    {
        buf[pos++] = hex[(Super.Uuid[i] >> 4) & 0xF];
        buf[pos++] = hex[Super.Uuid[i] & 0xF];
    }
    buf[pos] = '\0';
}

VNode* NanoFs::GetRoot()
{
    return VNodes[0];
}

// --- Inode I/O ---

bool NanoFs::ReadInode(u32 idx, NanoInode* out)
{
    if (idx >= NanoInodeCount)
    {
        Trace(0, "NanoFs::ReadInode: idx %u out of range", (ulong)idx);
        return false;
    }

    if (!Io.ReadBlock(Super.InodeStartBlock + idx, out))
    {
        Trace(0, "NanoFs::ReadInode: read block failed idx %u", (ulong)idx);
        return false;
    }
    return true;
}

bool NanoFs::WriteInode(u32 idx, const NanoInode* in)
{
    if (idx >= NanoInodeCount)
    {
        Trace(0, "NanoFs::WriteInode: idx %u out of range", (ulong)idx);
        return false;
    }

    if (!Io.WriteBlock(Super.InodeStartBlock + idx, in))
    {
        Trace(0, "NanoFs::WriteInode: write block failed idx %u", (ulong)idx);
        return false;
    }
    return true;
}

bool NanoFs::FlushSuper()
{
    ComputeSuperChecksum();
    if (!Io.WriteBlock(0, &Super, true))
    {
        Trace(0, "NanoFs::FlushSuper: write failed");
        return false;
    }
    return true;
}

// --- Bitmap helpers ---

long NanoFs::AllocInode()
{
    Stdlib::Bitmap bm(Super.InodeBitmap, NanoInodeCount);
    long idx = bm.FindSetZeroBit();
    if (idx < 0)
    {
        Trace(0, "NanoFs::AllocInode: no free inodes");
        return -1;
    }
    FlushSuper();
    return idx;
}

void NanoFs::FreeInode(u32 idx)
{
    if (idx >= NanoInodeCount)
        return;
    Stdlib::Bitmap bm(Super.InodeBitmap, NanoInodeCount);
    bm.ClearBit(idx);
    FlushSuper();
}

long NanoFs::AllocDataBlock()
{
    Stdlib::Bitmap bm(Super.DataBitmap, NanoDataBlockCount);
    long idx = bm.FindSetZeroBit();
    if (idx < 0)
    {
        Trace(0, "NanoFs::AllocDataBlock: no free data blocks");
        return -1;
    }
    FlushSuper();
    return idx;
}

void NanoFs::FreeDataBlock(u32 idx)
{
    if (idx >= NanoDataBlockCount)
        return;
    Stdlib::Bitmap bm(Super.DataBitmap, NanoDataBlockCount);
    bm.ClearBit(idx);
    FlushSuper();
}

// --- Checksum helpers ---

void NanoFs::ComputeSuperChecksum()
{
    Super.Checksum = 0;
    Super.Checksum = Stdlib::Crc32(&Super, sizeof(Super));
}

bool NanoFs::VerifySuperChecksum()
{
    u32 saved = Super.Checksum;
    Super.Checksum = 0;
    u32 computed = Stdlib::Crc32(&Super, sizeof(Super));
    Super.Checksum = saved;
    return computed == saved;
}

void NanoFs::ComputeInodeChecksum(NanoInode* inode)
{
    inode->Checksum = 0;
    inode->Checksum = Stdlib::Crc32(inode, sizeof(NanoInode));
}

bool NanoFs::VerifyInodeChecksum(NanoInode* inode)
{
    u32 saved = inode->Checksum;
    inode->Checksum = 0;
    u32 computed = Stdlib::Crc32(inode, sizeof(NanoInode));
    inode->Checksum = saved;
    return computed == saved;
}

u32 NanoFs::ComputeDataChecksum(NanoInode* inode)
{
    if (inode->Type != NanoInodeTypeFile || inode->Size == 0)
        return 0;

    u8* buf = (u8*)Mm::Alloc(NanoBlockSize, 0);
    if (buf == nullptr)
        return 0;

    u32 result = 0;
    u32 remaining = inode->Size;

    for (u32 i = 0; i < NanoMaxBlocks && remaining > 0; i++)
    {
        if (!Io.ReadBlock(Super.DataStartBlock + inode->Blocks[i], buf))
        {
            Mm::Free(buf);
            return 0;
        }

        u32 chunkSize = (remaining < NanoBlockSize) ? remaining : NanoBlockSize;
        result ^= Stdlib::Crc32(buf, chunkSize);
        remaining -= chunkSize;
    }

    Mm::Free(buf);
    return result;
}

// --- VNode management ---

VNode* NanoFs::LoadVNode(u32 inodeIdx)
{
    if (inodeIdx >= NanoInodeCount)
    {
        Trace(0, "NanoFs::LoadVNode: idx %u out of range", (ulong)inodeIdx);
        return nullptr;
    }

    if (VNodes[inodeIdx] != nullptr)
        return VNodes[inodeIdx];

    NanoInode* inode = new NanoInode();
    if (inode == nullptr)
    {
        Trace(0, "NanoFs::LoadVNode: alloc inode failed for %u", (ulong)inodeIdx);
        return nullptr;
    }

    if (!ReadInode(inodeIdx, inode))
    {
        Trace(0, "NanoFs::LoadVNode: read inode %u failed", (ulong)inodeIdx);
        delete inode;
        return nullptr;
    }

    if (inode->Type == NanoInodeTypeFree)
    {
        Trace(0, "NanoFs::LoadVNode: inode %u is free", (ulong)inodeIdx);
        delete inode;
        return nullptr;
    }

    if (!VerifyInodeChecksum(inode))
    {
        Trace(0, "NanoFs::LoadVNode: inode %u checksum mismatch", (ulong)inodeIdx);
        delete inode;
        return nullptr;
    }

    VNode* vnode = new VNode();
    if (vnode == nullptr)
    {
        Trace(0, "NanoFs::LoadVNode: alloc vnode failed for %u", (ulong)inodeIdx);
        delete inode;
        return nullptr;
    }

    Stdlib::MemSet(vnode, 0, sizeof(VNode));
    Stdlib::StrnCpy(vnode->Name, inode->Name, sizeof(vnode->Name));
    vnode->NodeType = (inode->Type == NanoInodeTypeDir) ? VNode::TypeDir : VNode::TypeFile;
    vnode->Parent = nullptr;
    vnode->Children.Init();
    vnode->SiblingLink.Init();
    vnode->Data = nullptr;
    vnode->Size = (inode->Type == NanoInodeTypeFile) ? inode->Size : 0;
    vnode->Capacity = inodeIdx; // Repurpose Capacity to store inode index

    VNodes[inodeIdx] = vnode;

    // If directory, load children
    if (inode->Type == NanoInodeTypeDir && inode->Size > 0)
    {
        // Read dir entries into a heap buffer to avoid 4 KB on stack per recursion level
        u8* dirBuf = (u8*)Mm::Alloc(NanoBlockSize, 0);
        if (dirBuf == nullptr)
        {
            Trace(0, "NanoFs::LoadVNode: alloc dir buf failed for inode %u", (ulong)inodeIdx);
        }
        else if (!Io.ReadBlock(Super.DataStartBlock + inode->Blocks[0], dirBuf))
        {
            Trace(0, "NanoFs::LoadVNode: read dir block failed for inode %u", (ulong)inodeIdx);
            Mm::Free(dirBuf);
            dirBuf = nullptr;
        }

        if (dirBuf != nullptr)
        {
            NanoDirEntry* entries = (NanoDirEntry*)dirBuf;
            for (u32 i = 0; i < inode->Size && i < NanoMaxDirEntries; i++)
            {
                VNode* child = LoadVNode(entries[i].InodeIndex);
                if (child != nullptr)
                {
                    child->Parent = vnode;
                    vnode->Children.InsertTail(&child->SiblingLink);
                }
            }
            Mm::Free(dirBuf);
        }
    }

    delete inode;
    return vnode;
}

VNode* NanoFs::FindVNode(u32 inodeIdx)
{
    if (inodeIdx >= NanoInodeCount)
        return nullptr;
    return VNodes[inodeIdx];
}

void NanoFs::FreeVNode(VNode* vnode)
{
    if (vnode == nullptr)
        return;

    u32 idx = VNodeToInode(vnode);
    if (idx < NanoInodeCount)
        VNodes[idx] = nullptr;

    vnode->SiblingLink.RemoveInit();
    delete vnode;
}

u32 NanoFs::VNodeToInode(VNode* vnode)
{
    if (vnode == nullptr)
        return (u32)-1;
    return (u32)vnode->Capacity; // Capacity stores inode index
}

// --- Directory helpers ---

bool NanoFs::AddDirEntry(u32 dirInodeIdx, u32 childInodeIdx)
{
    NanoInode* dirInode = new NanoInode();
    if (dirInode == nullptr)
    {
        Trace(0, "NanoFs::AddDirEntry: alloc inode failed");
        return false;
    }

    if (!ReadInode(dirInodeIdx, dirInode))
    {
        Trace(0, "NanoFs::AddDirEntry: read dir inode %u failed", (ulong)dirInodeIdx);
        delete dirInode;
        return false;
    }

    if (dirInode->Type != NanoInodeTypeDir)
    {
        Trace(0, "NanoFs::AddDirEntry: inode %u is not a dir", (ulong)dirInodeIdx);
        delete dirInode;
        return false;
    }

    if (dirInode->Size >= NanoMaxDirEntries)
    {
        Trace(0, "NanoFs::AddDirEntry: dir %u full (%u entries)", (ulong)dirInodeIdx, (ulong)dirInode->Size);
        delete dirInode;
        return false;
    }

    u8* dirBuf = (u8*)Mm::Alloc(NanoBlockSize, 0);
    if (dirBuf == nullptr)
    {
        Trace(0, "NanoFs::AddDirEntry: alloc dir buf failed");
        delete dirInode;
        return false;
    }

    if (!Io.ReadBlock(Super.DataStartBlock + dirInode->Blocks[0], dirBuf))
    {
        Trace(0, "NanoFs::AddDirEntry: read dir block failed for inode %u", (ulong)dirInodeIdx);
        Mm::Free(dirBuf);
        delete dirInode;
        return false;
    }

    NanoDirEntry* entries = (NanoDirEntry*)dirBuf;
    entries[dirInode->Size].InodeIndex = childInodeIdx;
    entries[dirInode->Size].Reserved = 0;

    dirInode->Size++;

    bool ok = Io.WriteBlock(Super.DataStartBlock + dirInode->Blocks[0], dirBuf);
    Mm::Free(dirBuf);

    if (!ok)
    {
        Trace(0, "NanoFs::AddDirEntry: write dir block failed for inode %u", (ulong)dirInodeIdx);
        delete dirInode;
        return false;
    }

    ComputeInodeChecksum(dirInode);
    ok = WriteInode(dirInodeIdx, dirInode);
    if (!ok)
        Trace(0, "NanoFs::AddDirEntry: write inode %u failed", (ulong)dirInodeIdx);
    delete dirInode;
    return ok;
}

bool NanoFs::RemoveDirEntry(u32 dirInodeIdx, u32 childInodeIdx)
{
    NanoInode* dirInode = new NanoInode();
    if (dirInode == nullptr)
    {
        Trace(0, "NanoFs::RemoveDirEntry: alloc inode failed");
        return false;
    }

    if (!ReadInode(dirInodeIdx, dirInode))
    {
        Trace(0, "NanoFs::RemoveDirEntry: read dir inode %u failed", (ulong)dirInodeIdx);
        delete dirInode;
        return false;
    }

    if (dirInode->Type != NanoInodeTypeDir)
    {
        Trace(0, "NanoFs::RemoveDirEntry: inode %u is not a dir", (ulong)dirInodeIdx);
        delete dirInode;
        return false;
    }

    u8* dirBuf = (u8*)Mm::Alloc(NanoBlockSize, 0);
    if (dirBuf == nullptr)
    {
        Trace(0, "NanoFs::RemoveDirEntry: alloc dir buf failed");
        delete dirInode;
        return false;
    }

    if (!Io.ReadBlock(Super.DataStartBlock + dirInode->Blocks[0], dirBuf))
    {
        Trace(0, "NanoFs::RemoveDirEntry: read dir block failed for inode %u", (ulong)dirInodeIdx);
        Mm::Free(dirBuf);
        delete dirInode;
        return false;
    }

    NanoDirEntry* entries = (NanoDirEntry*)dirBuf;

    // Find the entry
    bool found = false;
    for (u32 i = 0; i < dirInode->Size; i++)
    {
        if (entries[i].InodeIndex == childInodeIdx)
        {
            // Shift remaining entries
            for (u32 j = i; j + 1 < dirInode->Size; j++)
            {
                entries[j] = entries[j + 1];
            }
            Stdlib::MemSet(&entries[dirInode->Size - 1], 0, sizeof(NanoDirEntry));
            dirInode->Size--;
            found = true;
            break;
        }
    }

    if (!found)
    {
        Trace(0, "NanoFs::RemoveDirEntry: child inode %u not in dir %u", (ulong)childInodeIdx, (ulong)dirInodeIdx);
        Mm::Free(dirBuf);
        delete dirInode;
        return false;
    }

    bool ok = Io.WriteBlock(Super.DataStartBlock + dirInode->Blocks[0], dirBuf);
    Mm::Free(dirBuf);

    if (!ok)
    {
        Trace(0, "NanoFs::RemoveDirEntry: write dir block failed for inode %u", (ulong)dirInodeIdx);
        delete dirInode;
        return false;
    }

    ComputeInodeChecksum(dirInode);
    ok = WriteInode(dirInodeIdx, dirInode);
    if (!ok)
        Trace(0, "NanoFs::RemoveDirEntry: write inode %u failed", (ulong)dirInodeIdx);
    delete dirInode;
    return ok;
}

// --- FileSystem interface ---

VNode* NanoFs::Lookup(VNode* dir, const char* name)
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

VNode* NanoFs::CreateFile(VNode* dir, const char* name)
{
    if (dir == nullptr || name == nullptr)
    {
        Trace(0, "NanoFs::CreateFile: null dir or name");
        return nullptr;
    }

    if (dir->NodeType != VNode::TypeDir)
    {
        Trace(0, "NanoFs::CreateFile: parent is not a dir");
        return nullptr;
    }

    if (Lookup(dir, name) != nullptr)
    {
        Trace(0, "NanoFs::CreateFile: '%s' already exists", name);
        return nullptr;
    }

    u32 dirInodeIdx = VNodeToInode(dir);

    long inodeIdx = AllocInode();
    if (inodeIdx < 0)
        return nullptr;

    NanoInode* inode = new NanoInode();
    if (inode == nullptr)
    {
        Trace(0, "NanoFs::CreateFile: alloc inode failed for '%s'", name);
        FreeInode((u32)inodeIdx);
        return nullptr;
    }

    Stdlib::MemSet(inode, 0, sizeof(*inode));
    inode->Type = NanoInodeTypeFile;
    inode->Size = 0;
    Stdlib::StrnCpy(inode->Name, name, sizeof(inode->Name));
    inode->ParentInode = dirInodeIdx;
    inode->DataChecksum = 0;

    ComputeInodeChecksum(inode);
    if (!WriteInode((u32)inodeIdx, inode))
    {
        Trace(0, "NanoFs::CreateFile: write inode failed for '%s'", name);
        delete inode;
        FreeInode((u32)inodeIdx);
        return nullptr;
    }
    delete inode;

    if (!AddDirEntry(dirInodeIdx, (u32)inodeIdx))
    {
        Trace(0, "NanoFs::CreateFile: add dir entry failed for '%s'", name);
        FreeInode((u32)inodeIdx);
        return nullptr;
    }

    VNode* vnode = new VNode();
    if (vnode == nullptr)
    {
        Trace(0, "NanoFs::CreateFile: alloc vnode failed for '%s'", name);
        RemoveDirEntry(dirInodeIdx, (u32)inodeIdx);
        FreeInode((u32)inodeIdx);
        return nullptr;
    }

    Stdlib::MemSet(vnode, 0, sizeof(VNode));
    Stdlib::StrnCpy(vnode->Name, name, sizeof(vnode->Name));
    vnode->NodeType = VNode::TypeFile;
    vnode->Parent = dir;
    vnode->Children.Init();
    vnode->SiblingLink.Init();
    vnode->Data = nullptr;
    vnode->Size = 0;
    vnode->Capacity = (ulong)inodeIdx;

    VNodes[(u32)inodeIdx] = vnode;
    dir->Children.InsertTail(&vnode->SiblingLink);

    return vnode;
}

VNode* NanoFs::CreateDir(VNode* dir, const char* name)
{
    if (dir == nullptr || name == nullptr)
    {
        Trace(0, "NanoFs::CreateDir: null dir or name");
        return nullptr;
    }

    if (dir->NodeType != VNode::TypeDir)
    {
        Trace(0, "NanoFs::CreateDir: parent is not a dir");
        return nullptr;
    }

    if (Lookup(dir, name) != nullptr)
    {
        Trace(0, "NanoFs::CreateDir: '%s' already exists", name);
        return nullptr;
    }

    u32 dirInodeIdx = VNodeToInode(dir);

    long inodeIdx = AllocInode();
    if (inodeIdx < 0)
        return nullptr;

    long dataIdx = AllocDataBlock();
    if (dataIdx < 0)
    {
        FreeInode((u32)inodeIdx);
        return nullptr;
    }

    // Zero out the new directory data block
    u8* zeroBuf = (u8*)Mm::Alloc(NanoBlockSize, 0);
    if (zeroBuf == nullptr)
    {
        Trace(0, "NanoFs::CreateDir: alloc zero buf failed for '%s'", name);
        FreeDataBlock((u32)dataIdx);
        FreeInode((u32)inodeIdx);
        return nullptr;
    }
    Stdlib::MemSet(zeroBuf, 0, NanoBlockSize);
    bool zeroOk = Io.WriteBlock(Super.DataStartBlock + (u32)dataIdx, zeroBuf);
    Mm::Free(zeroBuf);
    if (!zeroOk)
    {
        Trace(0, "NanoFs::CreateDir: write zero block failed for '%s'", name);
        FreeDataBlock((u32)dataIdx);
        FreeInode((u32)inodeIdx);
        return nullptr;
    }

    NanoInode* inode = new NanoInode();
    if (inode == nullptr)
    {
        Trace(0, "NanoFs::CreateDir: alloc inode failed for '%s'", name);
        FreeDataBlock((u32)dataIdx);
        FreeInode((u32)inodeIdx);
        return nullptr;
    }

    Stdlib::MemSet(inode, 0, sizeof(*inode));
    inode->Type = NanoInodeTypeDir;
    inode->Size = 0;
    Stdlib::StrnCpy(inode->Name, name, sizeof(inode->Name));
    inode->ParentInode = dirInodeIdx;
    inode->Blocks[0] = (u32)dataIdx;
    inode->DataChecksum = 0;

    ComputeInodeChecksum(inode);
    if (!WriteInode((u32)inodeIdx, inode))
    {
        Trace(0, "NanoFs::CreateDir: write inode failed for '%s'", name);
        delete inode;
        FreeDataBlock((u32)dataIdx);
        FreeInode((u32)inodeIdx);
        return nullptr;
    }
    delete inode;

    if (!AddDirEntry(dirInodeIdx, (u32)inodeIdx))
    {
        Trace(0, "NanoFs::CreateDir: add dir entry failed for '%s'", name);
        FreeDataBlock((u32)dataIdx);
        FreeInode((u32)inodeIdx);
        return nullptr;
    }

    VNode* vnode = new VNode();
    if (vnode == nullptr)
    {
        Trace(0, "NanoFs::CreateDir: alloc vnode failed for '%s'", name);
        RemoveDirEntry(dirInodeIdx, (u32)inodeIdx);
        FreeDataBlock((u32)dataIdx);
        FreeInode((u32)inodeIdx);
        return nullptr;
    }

    Stdlib::MemSet(vnode, 0, sizeof(VNode));
    Stdlib::StrnCpy(vnode->Name, name, sizeof(vnode->Name));
    vnode->NodeType = VNode::TypeDir;
    vnode->Parent = dir;
    vnode->Children.Init();
    vnode->SiblingLink.Init();
    vnode->Data = nullptr;
    vnode->Size = 0;
    vnode->Capacity = (ulong)inodeIdx;

    VNodes[(u32)inodeIdx] = vnode;
    dir->Children.InsertTail(&vnode->SiblingLink);

    return vnode;
}

bool NanoFs::Write(VNode* file, const void* data, ulong len)
{
    if (file == nullptr || file->NodeType != VNode::TypeFile)
    {
        Trace(0, "NanoFs::Write: null file or not a file");
        return false;
    }

    if (len > NanoMaxFileSize)
    {
        Trace(0, "NanoFs::Write: len %u exceeds max %u", (ulong)len, (ulong)NanoMaxFileSize);
        return false;
    }

    u32 inodeIdx = VNodeToInode(file);
    NanoInode* inode = new NanoInode();
    if (inode == nullptr)
    {
        Trace(0, "NanoFs::Write: alloc inode failed");
        return false;
    }

    if (!ReadInode(inodeIdx, inode))
    {
        Trace(0, "NanoFs::Write: read inode %u failed", (ulong)inodeIdx);
        delete inode;
        return false;
    }

    u32 oldBlockCount = (inode->Size > 0)
        ? (inode->Size + NanoBlockSize - 1) / NanoBlockSize : 0;

    if (len == 0)
    {
        // Free old data blocks
        for (u32 i = 0; i < oldBlockCount; i++)
        {
            FreeDataBlock(inode->Blocks[i]);
            inode->Blocks[i] = 0;
        }
        inode->Size = 0;
        inode->DataChecksum = 0;
        ComputeInodeChecksum(inode);
        bool ok = WriteInode(inodeIdx, inode);
        delete inode;
        if (!ok)
        {
            Trace(0, "NanoFs::Write: truncate inode %u failed", (ulong)inodeIdx);
            return false;
        }
        file->Size = 0;
        return true;
    }

    // Allocate new data blocks into a temporary array first
    u32 newBlockCount = ((u32)len + NanoBlockSize - 1) / NanoBlockSize;
    u32 newBlocks[NanoMaxBlocks];
    Stdlib::MemSet(newBlocks, 0, sizeof(newBlocks));

    for (u32 i = 0; i < newBlockCount; i++)
    {
        long blk = AllocDataBlock();
        if (blk < 0)
        {
            Trace(0, "NanoFs::Write: alloc block %u/%u failed for inode %u",
                  (ulong)i, (ulong)newBlockCount, (ulong)inodeIdx);
            // Roll back already allocated new blocks
            for (u32 j = 0; j < i; j++)
                FreeDataBlock(newBlocks[j]);
            delete inode;
            return false;
        }
        newBlocks[i] = (u32)blk;
    }

    // Write data to new blocks
    u8* wbuf = (u8*)Mm::Alloc(NanoBlockSize, 0);
    if (wbuf == nullptr)
    {
        Trace(0, "NanoFs::Write: alloc write buf failed");
        for (u32 j = 0; j < newBlockCount; j++)
            FreeDataBlock(newBlocks[j]);
        delete inode;
        return false;
    }

    const u8* src = static_cast<const u8*>(data);
    u32 remaining = (u32)len;
    bool writeOk = true;
    for (u32 i = 0; i < newBlockCount; i++)
    {
        u32 chunkSize = (remaining < NanoBlockSize) ? remaining : NanoBlockSize;
        Stdlib::MemSet(wbuf, 0, NanoBlockSize);
        Stdlib::MemCpy(wbuf, src, chunkSize);

        if (!Io.WriteBlock(Super.DataStartBlock + newBlocks[i], wbuf))
        {
            Trace(0, "NanoFs::Write: write data block %u failed for inode %u",
                  (ulong)newBlocks[i], (ulong)inodeIdx);
            writeOk = false;
            break;
        }

        src += chunkSize;
        remaining -= chunkSize;
    }

    Mm::Free(wbuf);

    if (!writeOk)
    {
        Trace(0, "NanoFs::Write: data write failed, rolling back inode %u", (ulong)inodeIdx);
        // Roll back all new blocks
        for (u32 j = 0; j < newBlockCount; j++)
            FreeDataBlock(newBlocks[j]);
        delete inode;
        return false;
    }

    // Success path: free old blocks, commit new blocks into inode
    for (u32 i = 0; i < oldBlockCount; i++)
        FreeDataBlock(inode->Blocks[i]);

    Stdlib::MemSet(inode->Blocks, 0, sizeof(inode->Blocks));
    for (u32 i = 0; i < newBlockCount; i++)
        inode->Blocks[i] = newBlocks[i];

    inode->Size = (u32)len;
    inode->DataChecksum = ComputeDataChecksum(inode);
    ComputeInodeChecksum(inode);

    bool ok = WriteInode(inodeIdx, inode);
    delete inode;
    if (!ok)
    {
        Trace(0, "NanoFs::Write: commit inode %u failed", (ulong)inodeIdx);
        return false;
    }

    file->Size = len;
    return true;
}

bool NanoFs::Read(VNode* file, void* buf, ulong len, ulong offset)
{
    if (file == nullptr || file->NodeType != VNode::TypeFile)
    {
        Trace(0, "NanoFs::Read: null file or not a file");
        return false;
    }

    u32 inodeIdx = VNodeToInode(file);
    NanoInode* inode = new NanoInode();
    if (inode == nullptr)
    {
        Trace(0, "NanoFs::Read: alloc inode failed");
        return false;
    }

    if (!ReadInode(inodeIdx, inode))
    {
        Trace(0, "NanoFs::Read: read inode %u failed", (ulong)inodeIdx);
        delete inode;
        return false;
    }

    if (!VerifyInodeChecksum(inode))
    {
        Trace(0, "NanoFs::Read: inode %u checksum mismatch", (ulong)inodeIdx);
        delete inode;
        return false;
    }

    if (offset >= inode->Size)
    {
        Trace(0, "NanoFs::Read: offset %u beyond size %u inode %u",
              (ulong)offset, (ulong)inode->Size, (ulong)inodeIdx);
        delete inode;
        return false;
    }

    ulong avail = inode->Size - offset;
    ulong toRead = (len < avail) ? len : avail;

    u8* dst = static_cast<u8*>(buf);
    u32 blockOff = (u32)(offset / NanoBlockSize);
    u32 byteOff = (u32)(offset % NanoBlockSize);

    u8* blockBuf = (u8*)Mm::Alloc(NanoBlockSize, 0);
    if (blockBuf == nullptr)
    {
        Trace(0, "NanoFs::Read: alloc block buf failed");
        delete inode;
        return false;
    }

    ulong bytesRead = 0;
    bool ok = true;

    while (bytesRead < toRead && blockOff < NanoMaxBlocks)
    {
        if (!Io.ReadBlock(Super.DataStartBlock + inode->Blocks[blockOff], blockBuf))
        {
            Trace(0, "NanoFs::Read: read data block %u failed for inode %u",
                  (ulong)inode->Blocks[blockOff], (ulong)inodeIdx);
            ok = false;
            break;
        }

        u32 chunkSize = NanoBlockSize - byteOff;
        if (chunkSize > toRead - bytesRead)
            chunkSize = (u32)(toRead - bytesRead);

        Stdlib::MemCpy(dst + bytesRead, blockBuf + byteOff, chunkSize);
        bytesRead += chunkSize;
        byteOff = 0;
        blockOff++;
    }

    // Verify data checksum
    if (ok && inode->DataChecksum != 0)
    {
        u32 computed = ComputeDataChecksum(inode);
        if (computed != inode->DataChecksum)
        {
            Trace(0, "NanoFs: data checksum mismatch inode %u", (ulong)inodeIdx);
            ok = false;
        }
    }

    Mm::Free(blockBuf);
    delete inode;
    return ok;
}

bool NanoFs::RemoveRecursive(VNode* node)
{
    if (node == nullptr)
    {
        Trace(0, "NanoFs::RemoveRecursive: null node");
        return false;
    }

    u32 inodeIdx = VNodeToInode(node);

    if (node->NodeType == VNode::TypeDir)
    {
        // Recursively remove children
        while (!node->Children.IsEmpty())
        {
            Stdlib::ListEntry* entry = node->Children.Flink;
            VNode* child = CONTAINING_RECORD(entry, VNode, SiblingLink);
            if (!RemoveRecursive(child))
                return false;
        }
    }

    // Read inode from disk to find allocated blocks
    NanoInode* inode = new NanoInode();
    if (inode == nullptr)
    {
        Trace(0, "NanoFs::RemoveRecursive: alloc inode failed for %u", (ulong)inodeIdx);
    }
    if (inode != nullptr)
    {
        if (ReadInode(inodeIdx, inode))
        {
            if (inode->Type == NanoInodeTypeDir)
            {
                FreeDataBlock(inode->Blocks[0]);
            }
            else if (inode->Type == NanoInodeTypeFile && inode->Size > 0)
            {
                u32 blockCount = (inode->Size + NanoBlockSize - 1) / NanoBlockSize;
                for (u32 i = 0; i < blockCount; i++)
                    FreeDataBlock(inode->Blocks[i]);
            }
        }

        // Clear inode on disk
        Stdlib::MemSet(inode, 0, sizeof(*inode));
        ComputeInodeChecksum(inode);
        WriteInode(inodeIdx, inode);

        delete inode;
    }

    FreeInode(inodeIdx);
    FreeVNode(node);

    return true;
}

bool NanoFs::Remove(VNode* node)
{
    if (node == nullptr)
    {
        Trace(0, "NanoFs::Remove: null node");
        return false;
    }

    // Cannot remove root
    if (node->Parent == nullptr)
    {
        Trace(0, "NanoFs::Remove: cannot remove root");
        return false;
    }

    u32 inodeIdx = VNodeToInode(node);
    u32 parentIdx = VNodeToInode(node->Parent);

    // Remove from parent directory on disk
    if (!RemoveDirEntry(parentIdx, inodeIdx))
    {
        Trace(0, "NanoFs::Remove: remove dir entry failed inode %u from parent %u",
              (ulong)inodeIdx, (ulong)parentIdx);
        return false;
    }

    // Parent VNode Size stays 0 for directories; no update needed.

    if (!RemoveRecursive(node))
        return false;

    return Io.Flush();
}

}
