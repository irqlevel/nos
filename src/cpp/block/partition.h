#pragma once

#include "block_device.h"

namespace Kernel
{

struct MbrPartEntry
{
    u8  Status;
    u8  ChsFirst[3];
    u8  Type;
    u8  ChsLast[3];
    u32 LbaStart;
    u32 LbaSize;
} __attribute__((packed));

struct Mbr
{
    static const u16 ValidSignature = 0xAA55;
    static const ulong BootstrapSize = 446;
    static const ulong MaxParts = 4;

    u8  Bootstrap[BootstrapSize];
    MbrPartEntry Parts[MaxParts];
    u16 Signature;
} __attribute__((packed));

static_assert(sizeof(Mbr) == 512, "Invalid MBR size");

/* A GPT disk carries a protective MBR: one entry of this type covering the
   whole disk, so a tool that only understands MBR sees an occupied disk
   rather than an empty one. Finding it is what says to look at LBA 1. */
static const u8 MbrTypeGptProtective = 0xEE;

struct GptHeader
{
    /* "EFI PART", read back as a little-endian u64. */
    static const u64 ValidSignature = 0x5452415020494645ULL;
    static const u32 MinHeaderSize = 92;

    u64 Signature;
    u32 Revision;
    u32 HeaderSize;
    u32 HeaderCrc32;   /* over HeaderSize bytes, with this field zeroed */
    u32 Reserved;
    u64 CurrentLba;
    u64 BackupLba;
    u64 FirstUsableLba;
    u64 LastUsableLba;
    u8  DiskGuid[16];
    u64 PartEntryLba;
    u32 NumPartEntries;
    u32 PartEntrySize;
    u32 PartArrayCrc32;
} __attribute__((packed));

static_assert(sizeof(GptHeader) == 92, "Invalid GPT header size");

struct GptPartEntry
{
    u8  TypeGuid[16];   /* all zero means the slot is unused */
    u8  PartGuid[16];
    u64 FirstLba;
    u64 LastLba;        /* inclusive */
    u64 Attributes;
    u16 Name[36];       /* UTF-16LE, not used here */
} __attribute__((packed));

static_assert(sizeof(GptPartEntry) == 128, "Invalid GPT entry size");

class PartitionDevice : public BlockDevice
{
public:
    PartitionDevice();
    virtual ~PartitionDevice();

    bool Init(BlockDevice* parent, u64 startSector, u64 sectorCount, const char* name);

    virtual const char* GetName() override;
    virtual u64 GetCapacity() override;
    virtual u64 GetSectorSize() override;
    virtual bool Flush() override;
    virtual bool ReadSectors(u64 sector, void* buf, u32 count) override;
    virtual bool WriteSectors(u64 sector, const void* buf, u32 count, bool fua = false) override;

    static void ProbeAll();

    /* Probe block devices that appeared after ProbeAll ran. NVMe is
       registered by the Rust driver well after the virtio disks, and its
       partitions would otherwise never be looked at. Safe to call more than
       once: a device is probed at most one time. */
    static void ProbeNew();

private:
    PartitionDevice(const PartitionDevice& other) = delete;
    PartitionDevice(PartitionDevice&& other) = delete;
    PartitionDevice& operator=(const PartitionDevice& other) = delete;
    PartitionDevice& operator=(PartitionDevice&& other) = delete;

    static bool ProbeDevice(BlockDevice* dev);
    static bool ProbeMbr(BlockDevice* dev, const Mbr* mbr);
    static bool ProbeGpt(BlockDevice* dev);
    static bool AddPartition(BlockDevice* dev, u64 start, u64 count, ulong index);
    static bool AlreadyProbed(BlockDevice* dev);
    static bool IsOwnInstance(BlockDevice* dev);

    BlockDevice* Parent;
    u64 StartSector;
    u64 SectorCount;
    char Name[16];

    static const ulong MaxDisks = 8;
    /* GPT routinely declares 128 slots. Eight is what a disk here actually
       uses, and the cap is what bounds the static instance table. */
    static const ulong PartsPerDisk = 8;
    static const ulong MaxPartitions = MaxDisks * PartsPerDisk;

    static PartitionDevice Instances[MaxPartitions];
    static ulong InstanceCount;

    /* Whole disks already looked at, so ProbeNew can skip them. */
    static BlockDevice* ProbedDisks[MaxDisks];
    static ulong ProbedDiskCount;
};

}
