#include "partition.h"

#include <include/const.h>
#include <kernel/trace.h>
#include <lib/stdlib.h>
#include <lib/unique_ptr.h>
#include <lib/checksum.h>
#include <mm/new.h>

namespace Kernel
{

PartitionDevice PartitionDevice::Instances[MaxPartitions];
ulong PartitionDevice::InstanceCount;
BlockDevice* PartitionDevice::ProbedDisks[MaxDisks];
ulong PartitionDevice::ProbedDiskCount;

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
    if (sector > SectorCount || count > SectorCount - sector)
        return false;
    return Parent->ReadSectors(StartSector + sector, buf, count);
}

bool PartitionDevice::WriteSectors(u64 sector, const void* buf, u32 count, bool fua)
{
    if (sector > SectorCount || count > SectorCount - sector)
        return false;
    return Parent->WriteSectors(StartSector + sector, buf, count, fua);
}

bool PartitionDevice::ProbeDevice(BlockDevice* dev)
{
    Stdlib::UniquePtr<u8, Mm::FreeDeleter> buf(static_cast<u8*>(Mm::Alloc(Const::PageSize, 0)));
    if (!buf.Get())
        return false;

    if (!dev->ReadSectors(0, buf.Get(), 1))
        return false;

    auto* mbr = reinterpret_cast<Mbr*>(buf.Get());
    if (mbr->Signature != Mbr::ValidSignature)
        return false;

    /* A GPT disk puts a protective MBR here, claiming the whole disk under
       one entry of a reserved type. Taking that at face value would register
       a "partition" spanning the disk and hide every real one. */
    for (ulong i = 0; i < Mbr::MaxParts; i++)
    {
        if (mbr->Parts[i].Type == MbrTypeGptProtective)
            return ProbeGpt(dev);
    }

    return ProbeMbr(dev, mbr);
}

bool PartitionDevice::AddPartition(BlockDevice* dev, u64 start, u64 count,
    ulong index)
{
    if (start == 0)
    {
        /* Would alias the partition table itself. */
        Trace(0, "PartitionDevice: partition %u starts at LBA 0, skipped",
            index + 1);
        return true;
    }

    if (start + count > dev->GetCapacity())
    {
        Trace(0, "PartitionDevice: partition %u exceeds disk capacity",
            index + 1);
        return true;
    }

    if (InstanceCount >= MaxPartitions)
    {
        Trace(0, "PartitionDevice: max partitions reached");
        return false;
    }

    const char* parentName = dev->GetName();
    ulong parentLen = Stdlib::StrLen(parentName);

    char name[16];
    /* Two digits, because GPT gives more slots than a single one covers. */
    ulong number = index + 1;
    ulong extra = (number >= 10) ? 2 : 1;
    if (parentLen + extra >= sizeof(name))
        return true;

    Stdlib::MemCpy(name, parentName, parentLen);
    if (extra == 2)
    {
        name[parentLen] = (char)('0' + number / 10);
        name[parentLen + 1] = (char)('0' + number % 10);
    }
    else
    {
        name[parentLen] = (char)('0' + number);
    }
    name[parentLen + extra] = '\0';

    auto& inst = Instances[InstanceCount];
    new (&inst) PartitionDevice();
    if (!inst.Init(dev, start, count, name))
        return true;

    if (!BlockDeviceTable::GetInstance().Register(&inst))
        return true;

    Trace(0, "Partition %s: start %u size %u", name, (ulong)start,
        (ulong)count);

    InstanceCount++;
    return true;
}

bool PartitionDevice::ProbeMbr(BlockDevice* dev, const Mbr* mbr)
{
    for (ulong i = 0; i < Mbr::MaxParts; i++)
    {
        const auto& entry = mbr->Parts[i];
        if (entry.Type == 0 || entry.LbaSize == 0)
            continue;

        if (!AddPartition(dev, entry.LbaStart, entry.LbaSize, i))
            break;
    }

    return true;
}

bool PartitionDevice::ProbeGpt(BlockDevice* dev)
{
    u64 sectorSize = dev->GetSectorSize();
    if (sectorSize < sizeof(GptHeader) || sectorSize > Const::PageSize)
        return false;

    Stdlib::UniquePtr<u8, Mm::FreeDeleter> buf(
        static_cast<u8*>(Mm::Alloc(Const::PageSize, 0)));
    if (!buf.Get())
        return false;

    /* The primary header sits at LBA 1. The backup at the end of the disk is
       not consulted: a disk whose primary header is damaged is not one to
       start writing a log into on a guess. */
    if (!dev->ReadSectors(1, buf.Get(), 1))
        return false;

    auto* hdr = reinterpret_cast<GptHeader*>(buf.Get());
    if (hdr->Signature != GptHeader::ValidSignature)
        return false;

    if (hdr->HeaderSize < GptHeader::MinHeaderSize || hdr->HeaderSize > sectorSize)
        return false;

    /* The checksum is taken over the header with its own field zeroed, so it
       has to be put back before anything else reads it. */
    u32 storedCrc = hdr->HeaderCrc32;
    hdr->HeaderCrc32 = 0;
    u32 computed = Stdlib::Crc32(hdr, hdr->HeaderSize);
    hdr->HeaderCrc32 = storedCrc;

    if (computed != storedCrc)
    {
        Trace(0, "PartitionDevice: %s GPT header checksum mismatch",
            dev->GetName());
        return false;
    }

    if (hdr->PartEntrySize < sizeof(GptPartEntry) ||
        hdr->PartEntrySize > sectorSize)
        return false;

    ulong entries = hdr->NumPartEntries;
    if (entries > PartsPerDisk)
        entries = PartsPerDisk;

    u64 entryLba = hdr->PartEntryLba;
    u32 entrySize = hdr->PartEntrySize;
    ulong perSector = (ulong)(sectorSize / entrySize);
    if (perSector == 0)
        return false;

    Stdlib::UniquePtr<u8, Mm::FreeDeleter> ebuf(
        static_cast<u8*>(Mm::Alloc(Const::PageSize, 0)));
    if (!ebuf.Get())
        return false;

    for (ulong i = 0; i < entries; i++)
    {
        ulong sectorIndex = i / perSector;
        ulong within = i % perSector;

        /* One sector at a time rather than the whole array: the array is
           16 KiB at the usual 128 slots, and only the first few are ever
           used on a disk anyone here would prepare. */
        if (within == 0)
        {
            if (!dev->ReadSectors(entryLba + sectorIndex, ebuf.Get(), 1))
                break;
        }

        auto* entry = reinterpret_cast<GptPartEntry*>(
            ebuf.Get() + within * entrySize);

        /* An all-zero type GUID marks an unused slot. */
        bool used = false;
        for (ulong b = 0; b < sizeof(entry->TypeGuid); b++)
        {
            if (entry->TypeGuid[b] != 0)
            {
                used = true;
                break;
            }
        }
        if (!used)
            continue;

        if (entry->LastLba < entry->FirstLba)
            continue;

        u64 count = entry->LastLba - entry->FirstLba + 1;
        if (!AddPartition(dev, entry->FirstLba, count, i))
            break;
    }

    return true;
}

bool PartitionDevice::IsOwnInstance(BlockDevice* dev)
{
    for (ulong i = 0; i < InstanceCount; i++)
    {
        if (&Instances[i] == dev)
            return true;
    }
    return false;
}

bool PartitionDevice::AlreadyProbed(BlockDevice* dev)
{
    for (ulong i = 0; i < ProbedDiskCount; i++)
    {
        if (ProbedDisks[i] == dev)
            return true;
    }
    return false;
}

void PartitionDevice::ProbeNew()
{
    auto& table = BlockDeviceTable::GetInstance();

    /* The count is read once: ProbeDevice registers partitions, and probing
       those would be probing our own output. */
    ulong devCount = table.GetCount();

    for (ulong i = 0; i < devCount; i++)
    {
        BlockDevice* dev = table.GetDevice(i);
        if (dev == nullptr)
            continue;

        if (IsOwnInstance(dev) || AlreadyProbed(dev))
            continue;

        if (ProbedDiskCount >= MaxDisks)
        {
            Trace(0, "PartitionDevice: max disks reached");
            return;
        }

        ProbedDisks[ProbedDiskCount] = dev;
        ProbedDiskCount++;

        ProbeDevice(dev);
    }
}

void PartitionDevice::ProbeAll()
{
    /* The instance table is constructed once; there is no way to unregister
       from BlockDeviceTable, so re-initialising entries that are still
       registered would hand out devices pointing at freed state. */
    static bool initialised;
    if (BugOn(initialised))
        return;
    initialised = true;

    InstanceCount = 0;
    ProbedDiskCount = 0;

    for (ulong i = 0; i < MaxPartitions; i++)
        new (&Instances[i]) PartitionDevice();

    ProbeNew();

    Trace(0, "PartitionDevice: probed %u partitions", InstanceCount);
}

}
