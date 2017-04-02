#include "memory_map.h"
#include "trace.h"
#include "stdlib.h"

namespace Kernel
{

namespace Core
{

MemoryMap::MemoryMap()
    : Size(0)
{
}

bool MemoryMap::AddRegion(u64 addr, u64 len, u32 type)
{
    if (Size >= Shared::ArraySize(Region))
        return false;

    auto& region = Region[Size];
    region.Addr = addr;
    region.Len = len;
    region.Type = type;

    Size++;

    return true;
}

bool MemoryMap::FindRegion(ulong base, ulong& start, ulong& end)
{
    start = 0;
    end = 0;
    for (size_t i = 0; i < Size; i++)
    {
        auto& region = Region[i];
        if (region.Type != 1)
            continue;

        if (region.Addr + region.Len <= base)
            continue;

        ulong regionBase = (region.Addr < base) ? base : region.Addr;
        ulong regionLength = (region.Addr < base) ?
            (region.Len - (base - region.Addr)) : region.Len;

        if ((end - start) < regionLength)
        {
            start = regionBase;
            end = regionLength;
        }
    }

    if (end > start && start != 0)
        return true;

    return false;
}

MemoryMap::~MemoryMap()
{
}

}

}