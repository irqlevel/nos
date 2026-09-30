// The memory map (mm/memory_map.cpp): the regions the firmware reports --
// Multiboot2's e820 entries, the device tree's memory nodes -- and the
// kernel's own carve-outs, and the questions the page allocator asks of them
// before it hands a page out. A firmware's map is whatever it says: regions
// that overlap, that are empty, that run past the top of the address space.
// Every answer is held to the same question answered naively, page by page,
// in arithmetic that cannot wrap, over the regions as memory_map.h says it
// keeps them; and the free-page scan PageTable::GetFreePages runs is run
// over the map, which must never list a page a reserved region covers, and
// must always move forward.
#include "host.h"

#include <include/const.h>
/* The map is a singleton with a private constructor, which Reset runs again
   in place between two inputs. */
#define private public
#include <mm/memory_map.h>
#undef private

#include <new>

#include "fuzz.h"
#include "kernel.h"

namespace
{

using Kernel::Mm::MemoryMap;
typedef unsigned __int128 u128;

const ulong Page = Const::PageSize;

/* A region as the model keeps it: [Addr, End), End up to 2^64. */
struct Region
{
    u128 Addr;
    u128 End;
    bool Usable;
};

/* Addresses a firmware map has: pages, odd bytes, the edges. */
ulong Address(Fuzz::Input& in)
{
    switch (in.U8() % 10)
    {
    case 0:
        return in.U64();
    case 1:
        return ~0UL - in.U8() * Page - in.U8();
    case 2:
        return (1UL << 52) - in.U8() * Page + (in.U8() & 3) - 2;
    case 3:
        return in.U8() * Page + in.U8();
    default:
        return (in.U8() % 64) * Page;
    }
}

ulong Length(Fuzz::Input& in)
{
    switch (in.U8() % 6)
    {
    case 0:
        return in.Value64();
    case 1:
        return 0;
    default:
        return (in.U8() % 16) * Page + (in.U8() & 3) * 0x80;
    }
}

bool Overlaps(const std::vector<Region>& map, bool usable, u128 a, u128 end)
{
    for (const Region& r : map)
    {
        if (r.Usable == usable && r.Addr < r.End && a < r.End && r.Addr < end)
            return true;
    }
    return false;
}

bool Reserved(const std::vector<Region>& map, u128 page)
{
    return Overlaps(map, false, page, page + Page);
}

void Run(Fuzz::Input& in)
{
    auto& mmap = MemoryMap::GetInstance();
    std::vector<Region> map;

    /* The map: what the firmware said, and sometimes more than it holds.
       What memory_map.h says is kept of a region: nothing of an empty one
       or one past MaxPhysAddr, and of the rest what is below it. */
    const u128 top = MemoryMap::MaxPhysAddr;
    bool many = in.Chance(8);
    ulong count = many ? 70 : in.Below(14);
    std::vector<ulong> types;
    for (ulong i = 0; i < count; i++)
    {
        ulong addr = many ? in.U8() * Page : Address(in);
        ulong len = many ? Page : Length(in);
        ulong type = in.Chance(80) ? 2 + in.Below(4) : MemoryMap::UsableRamType;
        bool added = mmap.AddRegion(addr, len, type);
        if (len == 0 || addr >= top)
        {
            INVARIANT(added, "AddRegion(0x%lx, 0x%lx) refused a region it keeps nothing of", addr, len);
            Fuzz::Reached("a region kept nothing of");
            continue;
        }
        if (map.size() >= 64)
        {
            INVARIANT(!added, "AddRegion took region %lu into a full map", i);
            continue;
        }
        INVARIANT(added, "AddRegion refused region %lu of a map with room", i);
        u128 end = static_cast<u128>(addr) + len;
        if (end > top)
        {
            end = top;
            Fuzz::Reached("a region cut at the top");
        }
        map.push_back({addr, end, type == MemoryMap::UsableRamType});
        types.push_back(type);
    }
    Fuzz::Reached(map.size() >= 64 ? "a full map" : "a map");
    INVARIANT(mmap.GetRegionCount() == map.size(), "the map holds %lu regions, not %lu", mmap.GetRegionCount(),
        map.size());
    for (size_t i = 0; i < map.size(); i++)
    {
        ulong addr, len, type;
        INVARIANT(mmap.GetRegion(i, addr, len, type), "no region %lu", i);
        INVARIANT(addr == map[i].Addr && len == map[i].End - map[i].Addr && type == types[i],
            "region %lu is 0x%lx len 0x%lx type %lu", i, addr, len, type);
    }

    /* The free-page scan of page_table.cpp over [start, end): skip what
       GetReservedEnd says is reserved, list up to GetNextReservedStart. */
    ulong start = in.Chance(32) ? ((1UL << 52) - (in.U8() % 8) * Page) : (in.U8() % 64) * Page;
    ulong end = start + (1 + in.U8() % 64) * Page;
    ulong address = start;
    for (ulong steps = 0; address < end; steps++)
    {
        INVARIANT(steps <= 2 * (end - start) / Page + 2, "the free-page scan does not end: at 0x%lx", address);
        ulong reservedEnd = mmap.GetReservedEnd(address);
        if (reservedEnd != 0)
        {
            INVARIANT(Reserved(map, address), "GetReservedEnd(0x%lx) says reserved to 0x%lx: nothing reserves it",
                address, reservedEnd);
            INVARIANT(reservedEnd > address, "GetReservedEnd(0x%lx) = 0x%lx: the scan goes nowhere", address,
                reservedEnd);
            if (reservedEnd > end)
                reservedEnd = end;
            for (ulong a = address; a < reservedEnd; a += Page)
                INVARIANT(Reserved(map, a), "GetReservedEnd(0x%lx) skips page 0x%lx, which nothing reserves",
                    address, a);
            Fuzz::Reached("a reserved run skipped");
            address = reservedEnd;
            continue;
        }
        INVARIANT(!Reserved(map, address), "GetReservedEnd(0x%lx) = 0 for a reserved page", address);
        ulong runEnd = mmap.GetNextReservedStart(address, end);
        INVARIANT(runEnd > address, "GetNextReservedStart(0x%lx) = 0x%lx: the scan goes nowhere", address, runEnd);
        INVARIANT(runEnd <= end, "GetNextReservedStart(0x%lx, 0x%lx) = 0x%lx: past its limit", address, end, runEnd);
        for (; address < runEnd; address += Page)
            INVARIANT(!Reserved(map, address), "the free-page scan lists page 0x%lx, which is reserved", address);
        Fuzz::Reached("a free run listed");
    }

    /* The per-address questions. */
    while (in.More())
    {
        ulong a = Address(in);
        switch (in.U8() % 4)
        {
        case 0:
        {
            bool usable = false;
            for (const Region& r : map)
            {
                if (r.Usable && a >= r.Addr && a < r.End)
                    usable = true;
            }
            INVARIANT(mmap.IsUsableRam(a) == usable, "IsUsableRam(0x%lx) says %d", a, !usable);
            break;
        }
        case 1:
        {
            ulong len = 1 + in.U8() * Page;
            /* A question about bytes past the top of the space is nobody's. */
            if (static_cast<u128>(a) + len > (static_cast<u128>(1) << 64))
                break;
            bool reserved = Overlaps(map, false, a, static_cast<u128>(a) + len);
            INVARIANT(mmap.IsReserved(a, len) == reserved, "IsReserved(0x%lx, 0x%lx) says %d", a, len, !reserved);
            break;
        }
        case 2:
        {
            ulong e = a + (1 + in.U8()) * Page;
            if (e < a)
                break;
            bool usable = Overlaps(map, true, a, e);
            INVARIANT(mmap.HasUsableRamIn(a, e) == usable, "HasUsableRamIn(0x%lx, 0x%lx) says %d", a, e, !usable);
            break;
        }
        default:
        {
            u128 above = 0;
            for (const Region& r : map)
            {
                if (r.Usable && r.End > a)
                    above += r.End - ((r.Addr > a) ? r.Addr : static_cast<u128>(a));
            }
            INVARIANT(mmap.GetUsableRamBytesAbove(a) == static_cast<ulong>(above),
                "GetUsableRamBytesAbove(0x%lx) = 0x%lx, not 0x%lx", a, mmap.GetUsableRamBytesAbove(a),
                static_cast<ulong>(above));
            break;
        }
        }
    }

    u128 total = 0;
    u128 highest = 0;
    for (const Region& r : map)
    {
        if (!r.Usable)
            continue;
        total += r.End - r.Addr;
        if (r.End > highest)
            highest = r.End;
    }
    INVARIANT(mmap.GetUsableRamBytes() == static_cast<ulong>(total), "GetUsableRamBytes() = 0x%lx, not 0x%lx",
        mmap.GetUsableRamBytes(), static_cast<ulong>(total));
    INVARIANT(mmap.GetUsableRamEnd() == static_cast<ulong>(highest), "GetUsableRamEnd() = 0x%lx, not 0x%lx",
        mmap.GetUsableRamEnd(), static_cast<ulong>(highest));
}

void Reset()
{
    MemoryMap& mmap = MemoryMap::GetInstance();
    mmap.~MemoryMap();
    new (&mmap) MemoryMap();
}

}

const Fuzz::Target Fuzz::TheTarget = {"memmap", Reset, Run, 1024, 200000};
