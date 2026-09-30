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
    , LastUsableRegion(0)
{
}

bool MemoryMap::AddRegion(ulong addr, ulong len, ulong type)
{
    if (len == 0)
        return true;

    if (addr >= MaxPhysAddr)
    {
        Trace(0, "mm: region 0x%lX len 0x%lX type %lu is past the physical address space, dropped",
            addr, len, type);
        return true;
    }

    if (len > MaxPhysAddr - addr)
    {
        Trace(0, "mm: region 0x%lX len 0x%lX type %lu reaches past the physical address space, cut at 0x%lX",
            addr, len, type, MaxPhysAddr);
        len = MaxPhysAddr - addr;
    }

    if (Size >= Stdlib::ArraySize(Region))
        return false;

    auto& region = Region[Size];
    region.Addr = addr;
    region.Len = len;
    region.Type = type;

    Size++;

    return true;
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
    /* Up to the top of the space, for a range the caller let run past it */
    const ulong end = (len > ~0UL - phyAddr) ? ~0UL : phyAddr + len;

    for (size_t i = 0; i < Size; i++)
    {
        auto& region = Region[i];
        if (region.Type == UsableRamType)
            continue;

        if (phyAddr < (region.Addr + region.Len) &&
            region.Addr < end)
            return true;
    }

    return false;
}

bool MemoryMap::IsUsableRam(ulong phyAddr)
{
    /* Every temp mapping asks this, and a temp mapping is on the path of
       every page allocation and every page-table walk; a real server's map
       has twenty-odd regions to walk past. The queries come in bursts inside
       one region, so try the region that answered last before scanning. */
    long hint = LastUsableRegion.Get();
    if (hint >= 0 && (size_t)hint < Size)
    {
        auto& region = Region[hint];
        if (region.Type == UsableRamType &&
            phyAddr >= region.Addr && phyAddr < (region.Addr + region.Len))
            return true;
    }

    for (size_t i = 0; i < Size; i++)
    {
        auto& region = Region[i];
        if (region.Type != UsableRamType)
            continue;

        if (phyAddr >= region.Addr && phyAddr < (region.Addr + region.Len))
        {
            LastUsableRegion.Set((long)i);
            return true;
        }
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

ulong MemoryMap::GetReservedEnd(ulong phyAddr)
{
    ulong end = 0;

    /* Nothing is there to reserve (AddRegion) */
    if (phyAddr >= MaxPhysAddr)
        return 0;

    for (size_t i = 0; i < Size; i++)
    {
        auto& region = Region[i];
        if (region.Type == UsableRamType)
            continue;

        /* The overlap test IsReserved does, for the page at phyAddr. */
        if (phyAddr < (region.Addr + region.Len) &&
            region.Addr < (phyAddr + Const::PageSize))
        {
            ulong regionEnd = Stdlib::RoundUp(region.Addr + region.Len,
                Const::PageSize);
            if (regionEnd > end)
                end = regionEnd;
        }
    }

    /* Two reserved regions that touch are two calls: the caller re-asks at
       the end of the first and walks off the second. */
    return end;
}

ulong MemoryMap::GetNextReservedStart(ulong phyAddr, ulong limit)
{
    ulong next = limit;

    for (size_t i = 0; i < Size; i++)
    {
        auto& region = Region[i];
        if (region.Type == UsableRamType)
            continue;

        /* Rounded down: a region starting mid-page makes that whole page
           reserved, exactly as IsReserved would have said. */
        ulong start = Stdlib::RoundDown(region.Addr, Const::PageSize);
        if (start > phyAddr && start < next)
            next = start;
    }

    return next;
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