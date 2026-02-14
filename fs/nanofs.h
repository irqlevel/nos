#pragma once

#include <fs/filesystem.h>
#include <fs/block_io.h>

namespace Kernel
{

static const u32 NanoMagic         = 0x4E414E4F; // "NANO"
static const u32 NanoVersion       = 1;
static const u32 NanoBlockSize     = 4096;
static const u32 NanoInodeCount    = 1024;
static const u32 NanoDataBlockCount = 16384;
static const u32 NanoInodeStart    = 1;
static const u32 NanoDataStart     = 1 + NanoInodeCount; // 1025
static const u32 NanoMaxBlocks     = 256;
static const u32 NanoMaxDirEntries = 256;
static const u32 NanoMaxFileSize   = NanoMaxBlocks * NanoBlockSize; // 1 MB

struct NanoSuperBlock
{
    u32 Magic;
    u32 Version;
    u8  Uuid[16];
    u32 Checksum;
    u32 BlockSize;
    u32 InodeCount;
    u32 DataBlockCount;
    u32 InodeStartBlock;
    u32 DataStartBlock;
    u8  InodeBitmap[128];
    u8  DataBitmap[2048];
    u8  Padding[NanoBlockSize - 48 - 128 - 2048];
};

static_assert(sizeof(NanoSuperBlock) == NanoBlockSize, "NanoSuperBlock must be 4 KB");

struct NanoInode
{
    u32 Type;           // 0 = free, 1 = file, 2 = dir
    u32 Size;           // file: byte count, dir: entry count
    char Name[64];
    u32 ParentInode;
    u32 Checksum;       // CRC32 of this block (zeroed during computation)
    u32 DataChecksum;   // CRC32 of file data (0 for dirs/empty)
    u32 Blocks[NanoMaxBlocks];
    u8  Padding[NanoBlockSize - 4 - 4 - 64 - 4 - 4 - 4 - NanoMaxBlocks * 4];
};

static_assert(sizeof(NanoInode) == NanoBlockSize, "NanoInode must be 4 KB");

struct NanoDirEntry
{
    u32 InodeIndex;
    u32 Reserved;
};

static const u32 NanoInodeTypeFree = 0;
static const u32 NanoInodeTypeFile = 1;
static const u32 NanoInodeTypeDir  = 2;

class NanoFs : public FileSystem
{
public:
    NanoFs(BlockDevice* dev);
    virtual ~NanoFs();

    virtual const char* GetName() override;
    virtual bool Format(BlockDevice* dev) override;
    virtual void GetInfo(char* buf, ulong bufSize) override;
    virtual bool Mount() override;
    virtual void Unmount() override;
    virtual VNode* GetRoot() override;
    virtual VNode* Lookup(VNode* dir, const char* name) override;
    virtual VNode* CreateFile(VNode* dir, const char* name) override;
    virtual VNode* CreateDir(VNode* dir, const char* name) override;
    virtual bool Write(VNode* file, const void* data, ulong len) override;
    virtual bool Read(VNode* file, void* buf, ulong len, ulong offset) override;
    virtual bool Remove(VNode* node) override;

private:
    NanoFs(const NanoFs& other) = delete;
    NanoFs(NanoFs&& other) = delete;
    NanoFs& operator=(const NanoFs& other) = delete;
    NanoFs& operator=(NanoFs&& other) = delete;

    bool ReadInode(u32 idx, NanoInode* out);
    bool WriteInode(u32 idx, const NanoInode* in);
    bool FlushSuper();

    long AllocInode();
    void FreeInode(u32 idx);
    long AllocDataBlock();
    void FreeDataBlock(u32 idx);

    void ComputeSuperChecksum();
    bool VerifySuperChecksum();
    void ComputeInodeChecksum(NanoInode* inode);
    bool VerifyInodeChecksum(NanoInode* inode);
    u32  ComputeDataChecksum(NanoInode* inode);

    VNode* LoadVNode(u32 inodeIdx);
    VNode* FindVNode(u32 inodeIdx);
    void   FreeVNode(VNode* vnode);
    u32    VNodeToInode(VNode* vnode);

    bool AddDirEntry(u32 dirInodeIdx, u32 childInodeIdx);
    bool RemoveDirEntry(u32 dirInodeIdx, u32 childInodeIdx);
    bool RemoveRecursive(VNode* node);

    BlockIo Io;
    NanoSuperBlock Super;
    VNode* VNodes[NanoInodeCount]; // in-memory VNode cache by inode index
};

}
