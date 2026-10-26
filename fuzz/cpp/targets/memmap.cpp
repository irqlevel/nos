// The memory map (mm/memory_map.cpp): the regions the firmware reports --
// Multiboot2's e820 entries, the device tree's memory nodes -- and the
// kernel's own carve-outs, and the questions the page allocator asks of them
// before it hands a page out. A firmware's map is whatever it says: regions
// that overlap, that are empty, that run past the top of the address space.
// Every answer is held to the same question answered naively, page by page,
// in arithmetic that cannot wrap, over the regions as memory_map.h says it
// keeps them; and the free-page scan PageTable::get_free_pages runs is run
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

using kernel::mm::MemoryMap;
typedef unsigned __int128 u128;

const ulong PAGE = consts::PAGE_SIZE;

/* A region as the model keeps it: [Addr, End), End up to 2^64. */
struct Region
{
    u128 addr;
    u128 end;
    bool usable;
};

/* Addresses a firmware map has: pages, odd bytes, the edges. */
ulong pick_address(fuzz::Input& in)
{
    switch (in.u8() % 10)
    {
    case 0:
        return in.u64();
    case 1:
        return ~0UL - in.u8() * PAGE - in.u8();
    case 2:
        return (1UL << 52) - in.u8() * PAGE + (in.u8() & 3) - 2;
    case 3:
        return in.u8() * PAGE + in.u8();
    default:
        return (in.u8() % 64) * PAGE;
    }
}

ulong length(fuzz::Input& in)
{
    switch (in.u8() % 6)
    {
    case 0:
        return in.value64();
    case 1:
        return 0;
    default:
        return (in.u8() % 16) * PAGE + (in.u8() & 3) * 0x80;
    }
}

bool overlaps(const std::vector<Region>& map, bool usable, u128 a, u128 end)
{
    for (const Region& r : map)
    {
        if (r.usable == usable && r.addr < r.end && a < r.end && r.addr < end)
            return true;
    }
    return false;
}

bool reserved(const std::vector<Region>& map, u128 page)
{
    return overlaps(map, false, page, page + PAGE);
}

void run(fuzz::Input& in)
{
    auto& mmap = MemoryMap::get_instance();
    std::vector<Region> map;

    /* The map: what the firmware said, and sometimes more than it holds.
       What memory_map.h says is kept of a region: nothing of an empty one
       or one past MAX_PHYS_ADDR, and of the rest what is below it. */
    const u128 top = MemoryMap::MAX_PHYS_ADDR;
    bool many = in.chance(8);
    ulong count = many ? MemoryMap::MAX_REGIONS + 6 : in.below(14);
    std::vector<ulong> types;
    for (ulong i = 0; i < count; i++)
    {
        ulong addr = many ? in.u8() * PAGE : pick_address(in);
        ulong len = many ? PAGE : length(in);
        ulong type = in.chance(80) ? 2 + in.below(4) : MemoryMap::USABLE_RAM_TYPE;
        bool added = mmap.add_region(addr, len, type);
        if (len == 0 || addr >= top)
        {
            INVARIANT(added, "add_region(0x%lx, 0x%lx) refused a region it keeps nothing of", addr, len);
            fuzz::reached("a region kept nothing of");
            continue;
        }
        if (map.size() >= MemoryMap::MAX_REGIONS)
        {
            INVARIANT(!added, "add_region took region %lu into a full map", i);
            continue;
        }
        INVARIANT(added, "add_region refused region %lu of a map with room", i);
        u128 end = static_cast<u128>(addr) + len;
        if (end > top)
        {
            end = top;
            fuzz::reached("a region cut at the top");
        }
        map.push_back({addr, end, type == MemoryMap::USABLE_RAM_TYPE});
        types.push_back(type);
    }
    fuzz::reached(map.size() >= MemoryMap::MAX_REGIONS ? "a full map" : "a map");
    INVARIANT(mmap.get_region_count() == map.size(), "the map holds %lu regions, not %lu", mmap.get_region_count(),
        map.size());
    for (size_t i = 0; i < map.size(); i++)
    {
        ulong addr, len, type;
        INVARIANT(mmap.get_region(i, addr, len, type), "no region %lu", i);
        INVARIANT(addr == map[i].addr && len == map[i].end - map[i].addr && type == types[i],
            "region %lu is 0x%lx len 0x%lx type %lu", i, addr, len, type);
    }

    /* The free-page scan of page_table.cpp over [start, end): skip what
       get_reserved_end says is reserved, list up to get_next_reserved_start. */
    ulong start = in.chance(32) ? ((1UL << 52) - (in.u8() % 8) * PAGE) : (in.u8() % 64) * PAGE;
    ulong end = start + (1 + in.u8() % 64) * PAGE;
    ulong address = start;
    for (ulong steps = 0; address < end; steps++)
    {
        INVARIANT(steps <= 2 * (end - start) / PAGE + 2, "the free-page scan does not end: at 0x%lx", address);
        ulong reserved_end = mmap.get_reserved_end(address);
        if (reserved_end != 0)
        {
            INVARIANT(reserved(map, address), "get_reserved_end(0x%lx) says reserved to 0x%lx: nothing reserves it",
                address, reserved_end);
            INVARIANT(reserved_end > address, "get_reserved_end(0x%lx) = 0x%lx: the scan goes nowhere", address,
                reserved_end);
            if (reserved_end > end)
                reserved_end = end;
            for (ulong a = address; a < reserved_end; a += PAGE)
                INVARIANT(reserved(map, a), "get_reserved_end(0x%lx) skips page 0x%lx, which nothing reserves",
                    address, a);
            fuzz::reached("a reserved run skipped");
            address = reserved_end;
            continue;
        }
        INVARIANT(!reserved(map, address), "get_reserved_end(0x%lx) = 0 for a reserved page", address);
        ulong run_end = mmap.get_next_reserved_start(address, end);
        INVARIANT(run_end > address, "get_next_reserved_start(0x%lx) = 0x%lx: the scan goes nowhere", address, run_end);
        INVARIANT(run_end <= end, "get_next_reserved_start(0x%lx, 0x%lx) = 0x%lx: past its limit", address, end, run_end);
        for (; address < run_end; address += PAGE)
            INVARIANT(!reserved(map, address), "the free-page scan lists page 0x%lx, which is reserved", address);
        fuzz::reached("a free run listed");
    }

    /* The per-address questions. */
    while (in.more())
    {
        ulong a = pick_address(in);
        switch (in.u8() % 4)
        {
        case 0:
        {
            bool usable = false;
            for (const Region& r : map)
            {
                if (r.usable && a >= r.addr && a < r.end)
                    usable = true;
            }
            INVARIANT(mmap.is_usable_ram(a) == usable, "is_usable_ram(0x%lx) says %d", a, !usable);
            break;
        }
        case 1:
        {
            ulong len = 1 + in.u8() * PAGE;
            /* A question about bytes past the top of the space is nobody's. */
            if (static_cast<u128>(a) + len > (static_cast<u128>(1) << 64))
                break;
            bool reserved = overlaps(map, false, a, static_cast<u128>(a) + len);
            INVARIANT(mmap.is_reserved(a, len) == reserved, "is_reserved(0x%lx, 0x%lx) says %d", a, len, !reserved);
            break;
        }
        case 2:
        {
            ulong e = a + (1 + in.u8()) * PAGE;
            if (e < a)
                break;
            bool usable = overlaps(map, true, a, e);
            INVARIANT(mmap.has_usable_ram_in(a, e) == usable, "has_usable_ram_in(0x%lx, 0x%lx) says %d", a, e, !usable);
            break;
        }
        default:
        {
            u128 above = 0;
            for (const Region& r : map)
            {
                if (r.usable && r.end > a)
                    above += r.end - ((r.addr > a) ? r.addr : static_cast<u128>(a));
            }
            INVARIANT(mmap.get_usable_ram_bytes_above(a) == static_cast<ulong>(above),
                "get_usable_ram_bytes_above(0x%lx) = 0x%lx, not 0x%lx", a, mmap.get_usable_ram_bytes_above(a),
                static_cast<ulong>(above));
            break;
        }
        }
    }

    u128 total = 0;
    u128 highest = 0;
    for (const Region& r : map)
    {
        if (!r.usable)
            continue;
        total += r.end - r.addr;
        if (r.end > highest)
            highest = r.end;
    }
    INVARIANT(mmap.get_usable_ram_bytes() == static_cast<ulong>(total), "get_usable_ram_bytes() = 0x%lx, not 0x%lx",
        mmap.get_usable_ram_bytes(), static_cast<ulong>(total));
    INVARIANT(mmap.get_usable_ram_end() == static_cast<ulong>(highest), "get_usable_ram_end() = 0x%lx, not 0x%lx",
        mmap.get_usable_ram_end(), static_cast<ulong>(highest));
}

void reset()
{
    MemoryMap& mmap = MemoryMap::get_instance();
    mmap.~MemoryMap();
    new (&mmap) MemoryMap();
}

}

const fuzz::Target fuzz::THE_TARGET = {"memmap", reset, run, 1024, 200000};
