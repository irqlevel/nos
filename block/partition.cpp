#include "partition.h"

#include <kernel/trace.h>
#include <lib/stdlib.h>
#include <mm/new.h>

namespace Kernel
{

PartitionDevice PartitionDevice::Instances[MaxPartitions];
ulong PartitionDevice::InstanceCount;

PartitionDevice::PartitionDevice()
    : Parent(nullptr)
    , StartSector(0)
    , SectorCount(0)
{
    Name[0] = '\0';
}

PartitionDevice::~PartitionDevice()
{
}

bool PartitionDevice::Init(BlockDevice* parent, u64 startSector, u64 sectorCount, const char* name)
{
    if (!parent || sectorCount == 0)
        return false;

    Parent = parent;
    StartSector = startSector;
    SectorCount = sectorCount;

    ulong nameLen = Stdlib::StrLen(name);
    if (nameLen >= sizeof(Name))
        nameLen = sizeof(Name) - 1;
    Stdlib::MemCpy(Name, name, nameLen);
    Name[nameLen] = '\0';

    return true;
}

const char* PartitionDevice::GetName()
{
    return Name;
}

u64 PartitionDevice::GetCapacity()
{
    return SectorCount;
}

u64 PartitionDevice::GetSectorSize()
{
    return Parent->GetSectorSize();
}

bool PartitionDevice::Flush()
{
    return Parent->Flush();
}

bool PartitionDevice::ReadSectors(u64 sector, void* buf, u32 count)
{
    if (sector + count > SectorCount)
        return false;
    return Parent->ReadSectors(StartSector + sector, buf, count);
}

bool PartitionDevice::WriteSectors(u64 sector, const void* buf, u32 count, bool fua)
{
    if (sector + count > SectorCount)
        return false;
    return Parent->WriteSectors(StartSector + sector, buf, count, fua);
}

bool PartitionDevice::ProbeDevice(BlockDevice* dev)
{
    u8 buf[512];
    if (!dev->ReadSectors(0, buf, 1))
        return false;

    auto* mbr = reinterpret_cast<Mbr*>(buf);
    if (mbr->Signature != Mbr::ValidSignature)
        return false;

    const char* parentName = dev->GetName();
    ulong parentLen = Stdlib::StrLen(parentName);

    for (ulong i = 0; i < Mbr::MaxParts; i++)
    {
        auto& entry = mbr->Parts[i];
        if (entry.Type == 0 || entry.LbaSize == 0)
            continue;

        u64 endSector = (u64)entry.LbaStart + entry.LbaSize;
        if (endSector > dev->GetCapacity())
        {
            Trace(0, "PartitionDevice: partition %u exceeds disk capacity", i + 1);
            continue;
        }

        if (InstanceCount >= MaxPartitions)
        {
            Trace(0, "PartitionDevice: max partitions reached");
            return true;
        }

        char name[16];
        if (parentLen >= sizeof(name) - 2)
            continue;
        Stdlib::MemCpy(name, parentName, parentLen);
        name[parentLen] = (char)('1' + i);
        name[parentLen + 1] = '\0';

        auto& inst = Instances[InstanceCount];
        new (&inst) PartitionDevice();
        if (!inst.Init(dev, entry.LbaStart, entry.LbaSize, name))
            continue;

        if (!BlockDeviceTable::GetInstance().Register(&inst))
            continue;

        Trace(0, "Partition %s: type 0x%p start %u size %u",
            name, (ulong)entry.Type, (ulong)entry.LbaStart, (ulong)entry.LbaSize);

        InstanceCount++;
    }

    return true;
}

void PartitionDevice::ProbeAll()
{
    InstanceCount = 0;

    for (ulong i = 0; i < MaxPartitions; i++)
        new (&Instances[i]) PartitionDevice();

    /* Snapshot current device count to avoid probing partition devices
       that we register during this loop. */
    auto& table = BlockDeviceTable::GetInstance();
    ulong devCount = table.GetCount();

    for (ulong i = 0; i < devCount; i++)
    {
        BlockDevice* dev = table.GetDevice(i);
        if (dev)
            ProbeDevice(dev);
    }

    Trace(0, "PartitionDevice: probed %u partitions", InstanceCount);
}

}
