#include "memory_map.h"
#include <kernel/trace.h>
#include <lib/stdlib.h>

extern "C" char KernelEnd;
extern "C" char KernelStart;

namespace Kernel
{

namespace Mm
{

MemoryMap::MemoryMap()
    : Size(0)
{
}

bool MemoryMap::AddRegion(ulong addr, ulong len, ulong type)
{
    if (Size >= Stdlib::ArraySize(Region))
        return false;

    auto& region = Region[Size];
    region.Addr = addr;
    region.Len = len;
    region.Type = type;

    Size++;

    return true;
}

bool MemoryMap::FindRegion(ulong base, ulong limit, ulong& start, ulong& end)
{
    start = 0;
    end = 0;
    for (size_t i = 0; i < Size; i++)
    {
        auto& region = Region[i];
        if (region.Type != UsableRamType)
            continue;

        if (region.Len == 0)
            continue;

        if (region.Addr + region.Len <= base)
            continue;

        ulong regionBase = (region.Addr < base) ? base : region.Addr;
        ulong regionLength = (region.Addr < base) ?
            (region.Len - (base - region.Addr)) : region.Len;

        if ((regionBase + regionLength) > limit)
        {
            if (regionBase >= limit)
                continue;

            regionLength = limit - regionBase;
        }

        if ((end - start) < regionLength)
        {
            start = regionBase;
            end = regionBase + regionLength;
        }
    }

    if (end > start && start != 0)
        return true;

    return false;
}

MemoryMap::~MemoryMap()
{
}

ulong MemoryMap::GetKernelStart()
{
    return Stdlib::RoundDown((ulong)&KernelStart, Const::PageSize);
}

ulong MemoryMap::GetKernelEnd()
{
    return Stdlib::RoundUp((ulong)&KernelEnd, Const::PageSize);
}


bool MemoryMap::IsReserved(ulong phyAddr, ulong len)
{
    for (size_t i = 0; i < Size; i++)
    {
        auto& region = Region[i];
        if (region.Type == UsableRamType)
            continue;

        if (phyAddr < (region.Addr + region.Len) &&
            region.Addr < (phyAddr + len))
            return true;
    }

    return false;
}

bool MemoryMap::IsUsableRam(ulong phyAddr)
{
    for (size_t i = 0; i < Size; i++)
    {
        auto& region = Region[i];
        if (region.Type != UsableRamType)
            continue;

        if (phyAddr >= region.Addr && phyAddr < (region.Addr + region.Len))
            return true;
    }

    return false;
}

size_t MemoryMap::GetRegionCount()
{
    return Size;
}

bool MemoryMap::GetRegion(size_t index, ulong& addr, ulong& len, ulong& type)
{
    if (index >= Size)
        return false;

    auto& region = Region[index];
    addr = region.Addr;
    len = region.Len;
    type = region.Type;
    return true;
}

const char* MemoryMap::GetRegionTypeName(ulong type)
{
    /* e820 / EFI-derived types as Multiboot2 and the FDT parser hand them
       over; anything else is firmware being creative and is treated as
       reserved either way. */
    static const ulong AcpiReclaimableType = 3;
    static const ulong AcpiNvsType = 4;
    static const ulong BadRamType = 5;

    switch (type)
    {
    case UsableRamType:        return "usable";
    case ReservedType:         return "reserved";
    case AcpiReclaimableType:  return "acpi-reclaim";
    case AcpiNvsType:          return "acpi-nvs";
    case BadRamType:           return "bad";
    default:                   return "unknown";
    }
}

ulong MemoryMap::GetUsableRamBytes()
{
    ulong total = 0;
    for (size_t i = 0; i < Size; i++)
    {
        if (Region[i].Type == UsableRamType)
            total += Region[i].Len;
    }

    return total;
}

ulong MemoryMap::GetUsableRamEnd()
{
    ulong end = 0;
    for (size_t i = 0; i < Size; i++)
    {
        if (Region[i].Type != UsableRamType)
            continue;

        ulong regionEnd = Region[i].Addr + Region[i].Len;
        if (regionEnd > end)
            end = regionEnd;
    }

    return end;
}

bool MemoryMap::HasUsableRamIn(ulong start, ulong end)
{
    for (size_t i = 0; i < Size; i++)
    {
        if (Region[i].Type != UsableRamType)
            continue;

        if (Region[i].Addr < end && (Region[i].Addr + Region[i].Len) > start)
            return true;
    }

    return false;
}

ulong MemoryMap::GetUsableRamBytesAbove(ulong limit)
{
    ulong total = 0;
    for (size_t i = 0; i < Size; i++)
    {
        if (Region[i].Type != UsableRamType)
            continue;

        ulong end = Region[i].Addr + Region[i].Len;
        if (end <= limit)
            continue;

        ulong start = (Region[i].Addr < limit) ? limit : Region[i].Addr;
        total += end - start;
    }

    return total;
}

}
}