#pragma once

#include <fs/filesystem.h>
#include <fs/block_io.h>

namespace Kernel
{

static const u16 Ext2Magic = 0xEF53;

static const u16 Ext2InodeModeDir  = 0x4000;
static const u16 Ext2InodeModeFile = 0x8000;

static const u8 Ext2DirTypeFile = 1;
static const u8 Ext2DirTypeDir  = 2;

static const u32 Ext2RootInode = 2;

static const u32 Ext2DirectBlocks   = 12;
static const u32 Ext2IndirectBlock  = 12;
static const u32 Ext2DIndirectBlock = 13;

static const u32 Ext2SuperBlockOffset = 1024;

struct Ext2SuperBlock
{
    u32 InodeCount;
    u32 BlockCount;
    u32 ReservedBlockCount;
    u32 FreeBlockCount;
    u32 FreeInodeCount;
    u32 FirstDataBlock;
    u32 LogBlockSize;
    u32 LogFragSize;
    u32 BlocksPerGroup;
    u32 FragsPerGroup;
    u32 InodesPerGroup;
    u32 MountTime;
    u32 WriteTime;
    u16 MountCount;
    u16 MaxMountCount;
    u16 Magic;
    u16 State;
    u16 Errors;
    u16 MinorRevLevel;
    u32 LastCheck;
    u32 CheckInterval;
    u32 CreatorOs;
    u32 RevLevel;
    u16 DefResUid;
    u16 DefResGid;
    /* Rev 1+ fields */
    u32 FirstInode;
    u16 InodeSize;
    u16 BlockGroupNr;
    u32 FeatureCompat;
    u32 FeatureIncompat;
    u32 FeatureRoCompat;
    u8  Uuid[16];
    char VolumeName[16];
    u8  Padding[888]; /* pad to 1024 bytes total */
};

static_assert(sizeof(Ext2SuperBlock) == 1024, "Ext2SuperBlock must be 1024 bytes");

struct Ext2GroupDesc
{
    u32 BlockBitmap;
    u32 InodeBitmap;
    u32 InodeTable;
    u16 FreeBlockCount;
    u16 FreeInodeCount;
    u16 UsedDirsCount;
    u16 Pad;
    u8  Reserved[12];
};

static_assert(sizeof(Ext2GroupDesc) == 32, "Ext2GroupDesc must be 32 bytes");

struct Ext2Inode
{
    u16 Mode;
    u16 Uid;
    u32 Size;
    u32 AccessTime;
    u32 CreateTime;
    u32 ModifyTime;
    u32 DeleteTime;
    u16 Gid;
    u16 LinksCount;
    u32 Blocks;
    u32 Flags;
    u32 Osd1;
    u32 Block[15];
    u32 Generation;
    u32 FileAcl;
    u32 DirAcl;
    u32 FragAddr;
    u8  Osd2[12];
};

static_assert(sizeof(Ext2Inode) == 128, "Ext2Inode must be 128 bytes");

struct Ext2DirEntry
{
    u32 Inode;
    u16 RecLen;
    u8  NameLen;
    u8  FileType;
    char Name[]; /* variable length */
};

class Ext2Fs : public FileSystem
{
public:
    Ext2Fs(BlockDevice* dev);
    virtual ~Ext2Fs();

    virtual const char* GetName() override;
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
    virtual BlockDevice* GetDevice() override;

private:
    Ext2Fs(const Ext2Fs& other) = delete;
    Ext2Fs(Ext2Fs&& other) = delete;
    Ext2Fs& operator=(const Ext2Fs& other) = delete;
    Ext2Fs& operator=(Ext2Fs&& other) = delete;

    bool ReadInode(u32 inodeNum, Ext2Inode* out);
    bool ReadInodeData(Ext2Inode* inode, void* buf, ulong len, ulong offset);
    u32  GetBlockNum(Ext2Inode* inode, u32 logicalBlock);
    bool ReadBlock(u32 blockNum, void* buf);
    VNode* LoadDir(u32 inodeNum);
    VNode* FindVNode(u32 inodeNum);
    void   FreeVNode(VNode* vnode);

    static const ulong MaxCachedVNodes = 512;

    struct VNodeEntry
    {
        u32 InodeNum;
        VNode* Node;
    };

    BlockDevice* Dev;
    Ext2SuperBlock* Super;
    Ext2GroupDesc* GroupDescs;
    u32 BlockSize;
    u32 GroupCount;
    u32 InodeSize;
    VNode* RootNode;
    VNodeEntry VNodes[MaxCachedVNodes];
    ulong VNodeCount;
    bool Mounted;
    u8* TmpBlock; /* page-aligned temp buffer for block reads */
};

}
