#pragma once

#include <fs/filesystem.h>

namespace Kernel
{

static const u16 Ext2Magic = 0xEF53;

static const u16 Ext2InodeModeTypeMask = 0xF000;
static const u16 Ext2InodeModeDir  = 0x4000;
static const u16 Ext2InodeModeFile = 0x8000;
/* What a file and a directory made here get: 0644 and 0755 */
static const u16 Ext2InodeModeFileDefault = Ext2InodeModeFile | 0x01A4;
static const u16 Ext2InodeModeDirDefault  = Ext2InodeModeDir | 0x01ED;

static const u8 Ext2DirTypeUnknown = 0;
static const u8 Ext2DirTypeFile = 1;
static const u8 Ext2DirTypeDir  = 2;

static const u32 Ext2RootInode = 2;

/* FeatureIncompat bits. FileType is required: LoadDir keys directory
   detection off the dirent FileType byte, which without this feature is the
   high half of a 16-bit NameLen. Any other incompat bit (ext3 journal
   recovery, ext4 extents/64bit, meta_bg, ...) changes the on-disk format in
   ways this driver cannot parse and must refuse -- ext3/ext4 share the same
   magic, so the magic check alone does not reject them. */
static const u32 Ext2IncompatFileType  = 0x0002;
static const u32 Ext2IncompatSupported = Ext2IncompatFileType;

/* FeatureRoCompat bits this driver maintains when writing. SparseSuper only
   changes where the backup superblocks are (this driver updates the primary
   alone, as Linux does); LargeFile allows a 64-bit size, which is refused
   rather than produced. Anything else -- gdt_csum with its group checksums
   and uninitialised-group flags above all -- would be silently broken by a
   write, so an image carrying it is mounted read-only. */
static const u32 Ext2RoCompatSparseSuper = 0x0001;
static const u32 Ext2RoCompatLargeFile   = 0x0002;
static const u32 Ext2RoCompatWritable    = Ext2RoCompatSparseSuper | Ext2RoCompatLargeFile;

/* Superblock State */
static const u16 Ext2StateValid = 0x0001;

/* Inode Flags: an htree-indexed directory (compat dir_index). This driver
   modifies directories linearly and clears the flag when it does, which is
   what Linux expects of a writer that does not maintain the index. */
static const u32 Ext2InodeFlagIndex = 0x00001000;

/* BlockSize = 1024 << LogBlockSize must not exceed PageSize (4096). */
static const u32 Ext2MaxLogBlockSize = 2;

static const u32 Ext2DirectBlocks   = 12;
static const u32 Ext2IndirectBlock  = 12;
static const u32 Ext2DIndirectBlock = 13;
static const u32 Ext2TIndirectBlock = 14;

static const u32 Ext2SuperBlockOffset = 1024;

/* Inode.Blocks counts units of this many bytes, whatever the block size */
static const u32 Ext2BlocksUnit = 512;

static const ulong Ext2DirEntryHeader = 8;
static const ulong Ext2DirEntryAlign = 4;
static const ulong Ext2MaxNameLen = 255;

/* Blocks a truncate releases between two inode commits (see Truncate) */
static const ulong Ext2FreeBatch = 512;

/* Recursion cap for a recursive remove (32 KB kernel stack) */
static const u32 Ext2MaxDirDepth = 32;

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

/* What Probe reads off an unmounted ext2 superblock: enough to pick a root
   filesystem by label or UUID. */
struct Ext2Identity
{
    u8 Uuid[16];
    char Label[17];
};

class Ext2Fs : public FileSystem
{
public:
    Ext2Fs(BlockDevice* dev);
    virtual ~Ext2Fs();

    /* Does dev carry an ext2 superblock? Fills id when it does. */
    static bool Probe(BlockDevice* dev, Ext2Identity& id);

    virtual const char* GetName() override;
    virtual void GetInfo(char* buf, ulong bufSize) override;
    virtual bool Mount() override;
    virtual void Unmount() override;
    virtual VNode* GetRoot() override;
    virtual bool LoadDir(VNode* dir) override;
    virtual VNode* Lookup(VNode* dir, const char* name) override;
    virtual VNode* CreateFile(VNode* dir, const char* name) override;
    virtual VNode* CreateDir(VNode* dir, const char* name) override;
    virtual bool Write(VNode* file, const void* data, ulong len, ulong offset) override;
    virtual bool Read(VNode* file, void* buf, ulong len, ulong offset) override;
    virtual bool Truncate(VNode* file, ulong size) override;
    virtual bool Rename(VNode* node, VNode* newDir, const char* newName) override;
    virtual bool Remove(VNode* node) override;
    virtual bool Sync() override;
    virtual BlockDevice* GetDevice() override;

private:
    Ext2Fs(const Ext2Fs& other) = delete;
    Ext2Fs(Ext2Fs&& other) = delete;
    Ext2Fs& operator=(const Ext2Fs& other) = delete;
    Ext2Fs& operator=(Ext2Fs&& other) = delete;

    /* Block I/O */
    bool ReadBlock(u32 blockNum, void* buf);
    bool WriteBlock(u32 blockNum, const void* buf, bool fua);
    bool IsDataBlock(u32 blockNum);
    bool ZeroBlock(u32 blockNum);

    /* Superblock and group descriptors */
    static bool ReadSuperBlock(BlockDevice* dev, u8* scratch, Ext2SuperBlock* out);
    bool WriteSuper();
    bool WriteGroupDescs();
    bool CommitMeta();
    u32 BlocksInGroup(u32 group);

    /* Inodes */
    bool ReadInode(u32 inodeNum, Ext2Inode* out);
    bool WriteInode(u32 inodeNum, const Ext2Inode* in);
    u32 InodeGroup(u32 inodeNum);
    void InitInode(Ext2Inode* inode, u16 mode);

    /* Allocation. The bitmap bits change in memory and reach the disk in
       FlushBitmap; the counts in the group descriptors and the superblock
       reach it in CommitMeta. */
    bool LoadBitmap(u32 blockNum);
    bool FlushBitmap();
    long AllocBlock(u32 goalGroup);
    bool FreeBlock(u32 blockNum);
    long AllocInode(u32 goalGroup, bool isDir);
    bool FreeInode(u32 inodeNum, bool isDir);

    /* Block mapping */
    bool LoadIndirect(u32 blockNum, u8* buf, u32& cached);
    bool WriteIndirect(u32 blockNum, const u8* buf);
    bool MapBlock(Ext2Inode* inode, u32 goalGroup, u32 logicalBlock, bool allocate,
                  u32& physBlock, bool& fresh);
    bool ReleaseTail(Ext2Inode* inode, u32 fromBlock, u32* batch, ulong& batchCount, u32& lastKept);

    /* Data */
    bool ReadInodeData(Ext2Inode* inode, void* buf, ulong len, ulong offset);
    bool WriteInodeData(Ext2Inode* inode, u32 goalGroup, const void* data, ulong len, ulong offset);
    bool TruncateInode(u32 inodeNum, Ext2Inode* inode, ulong newSize);

    /* Directories */
    bool AddDirEntry(VNode* dir, Ext2Inode* dirInode, u32 inodeNum, const char* name, u8 fileType);
    bool RemoveDirEntry(VNode* dir, Ext2Inode* dirInode, u32 inodeNum, const char* name);
    bool SetDotDot(Ext2Inode* dirInode, u32 parentInodeNum);
    bool LinkCountAdjust(u32 inodeNum, int delta);
    bool RemoveNode(VNode* node, u32 depth);

    /* VNodes */
    VNode* NewVNode(VNode* parent, const char* name, VNode::Type type, u32 inodeNum, ulong size);
    void FreeTree(VNode* node);

    BlockDevice* Dev;
    Ext2SuperBlock* Super;
    Ext2GroupDesc* GroupDescs;
    ulong GdtBlocks;
    u32 GdtBlock;
    u32 BlockSize;
    u32 GroupCount;
    u32 InodeSize;
    u32 PtrsPerBlock;
    VNode* RootNode;
    bool Mounted;
    bool MetaDirty;        /* superblock/group descriptor counts changed in memory */

    /* Page-aligned scratch buffers, one block each. TmpBlock serves the
       inode table, the superblock and directory blocks; DataBuf the data
       block read-modify-write; IndBuf and DindBuf cache the indirect and
       doubly-indirect block last used, so a sequential pass over a file
       does not re-read them per data block; BitmapBuf holds the bitmap
       block being allocated from. */
    u8* TmpBlock;
    u8* DataBuf;
    u8* IndBuf;
    u32 IndBufBlock;
    u8* DindBuf;
    u32 DindBufBlock;
    u8* BitmapBuf;
    u32 BitmapBlock;
    bool BitmapDirty;
};

}
