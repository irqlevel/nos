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

class PartitionDevice : public BlockDevice
{
public:
    PartitionDevice();
    virtual ~PartitionDevice();

    bool Init(BlockDevice* parent, u64 startSector, u64 sectorCount, const char* name);

    virtual const char* GetName() override;
    virtual u64 GetCapacity() override;
    virtual u64 GetSectorSize() override;
    virtual bool ReadSector(u64 sector, void* buf) override;
    virtual bool WriteSector(u64 sector, const void* buf) override;

    static void ProbeAll();

private:
    PartitionDevice(const PartitionDevice& other) = delete;
    PartitionDevice(PartitionDevice&& other) = delete;
    PartitionDevice& operator=(const PartitionDevice& other) = delete;
    PartitionDevice& operator=(PartitionDevice&& other) = delete;

    static bool ProbeDevice(BlockDevice* dev);

    BlockDevice* Parent;
    u64 StartSector;
    u64 SectorCount;
    char Name[16];

    static const ulong MaxDisks = 8;
    static const ulong PartsPerDisk = 4;
    static const ulong MaxPartitions = MaxDisks * PartsPerDisk;

    static PartitionDevice Instances[MaxPartitions];
    static ulong InstanceCount;
};

}
