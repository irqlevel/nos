#include "memory_map.h"
#include <kernel/trace.h>
#include <lib/stdlib.h>

extern "C" char kernel_end;
extern "C" char kernel_start;

namespace kernel
{

namespace mm
{

MemoryMap::MemoryMap()
    : size_(0)
    , last_usable_region_(0)
{
}

bool MemoryMap::add_region(ulong addr, ulong len, ulong type)
{
    if (len == 0)
        return true;

    if (addr >= MAX_PHYS_ADDR)
    {
        trace(0, "mm: region 0x%lX len 0x%lX type %lu is past the physical address space, dropped",
            addr, len, type);
        return true;
    }

    if (len > MAX_PHYS_ADDR - addr)
    {
        trace(0, "mm: region 0x%lX len 0x%lX type %lu reaches past the physical address space, cut at 0x%lX",
            addr, len, type, MAX_PHYS_ADDR);
        len = MAX_PHYS_ADDR - addr;
    }

    if (size_ >= stdlib::array_size(region_))
        return false;

    auto& region = region_[size_];
    region.addr = addr;
    region.len = len;
    region.type = type;

    size_++;

    return true;
}

MemoryMap::~MemoryMap()
{
}

ulong MemoryMap::get_kernel_start()
{
    return stdlib::round_down((ulong)&kernel_start, consts::PAGE_SIZE);
}

ulong MemoryMap::get_kernel_end()
{
    return stdlib::round_up((ulong)&kernel_end, consts::PAGE_SIZE);
}


bool MemoryMap::is_reserved(ulong phy_addr, ulong len)
{
    /* Up to the top of the space, for a range the caller let run past it */
    const ulong end = (len > ~0UL - phy_addr) ? ~0UL : phy_addr + len;

    for (size_t i = 0; i < size_; i++)
    {
        auto& region = region_[i];
        if (region.type == USABLE_RAM_TYPE)
            continue;

        if (phy_addr < (region.addr + region.len) &&
            region.addr < end)
            return true;
    }

    return false;
}

bool MemoryMap::is_usable_ram(ulong phy_addr)
{
    /* Every temp mapping asks this, and a temp mapping is on the path of
       every page allocation and every page-table walk; a real server's map
       has twenty-odd regions to walk past. The queries come in bursts inside
       one region, so try the region that answered last before scanning. */
    long hint = last_usable_region_.get();
    if (hint >= 0 && (size_t)hint < size_)
    {
        auto& region = region_[hint];
        if (region.type == USABLE_RAM_TYPE &&
            phy_addr >= region.addr && phy_addr < (region.addr + region.len))
            return true;
    }

    for (size_t i = 0; i < size_; i++)
    {
        auto& region = region_[i];
        if (region.type != USABLE_RAM_TYPE)
            continue;

        if (phy_addr >= region.addr && phy_addr < (region.addr + region.len))
        {
            last_usable_region_.set((long)i);
            return true;
        }
    }

    return false;
}

size_t MemoryMap::get_region_count()
{
    return size_;
}

bool MemoryMap::get_region(size_t index, ulong& addr, ulong& len, ulong& type)
{
    if (index >= size_)
        return false;

    auto& region = region_[index];
    addr = region.addr;
    len = region.len;
    type = region.type;
    return true;
}

const char* MemoryMap::get_region_type_name(ulong type)
{
    /* e820 / EFI-derived types as Multiboot2 and the FDT parser hand them
       over; anything else is firmware being creative and is treated as
       reserved either way. */
    static const ulong ACPI_RECLAIMABLE_TYPE = 3;
    static const ulong ACPI_NVS_TYPE = 4;
    static const ulong BAD_RAM_TYPE = 5;

    switch (type)
    {
    case USABLE_RAM_TYPE:        return "usable";
    case RESERVED_TYPE:         return "reserved";
    case ACPI_RECLAIMABLE_TYPE:  return "acpi-reclaim";
    case ACPI_NVS_TYPE:          return "acpi-nvs";
    case BAD_RAM_TYPE:           return "bad";
    default:                   return "unknown";
    }
}

ulong MemoryMap::get_reserved_end(ulong phy_addr)
{
    ulong end = 0;

    /* Nothing is there to reserve (add_region) */
    if (phy_addr >= MAX_PHYS_ADDR)
        return 0;

    for (size_t i = 0; i < size_; i++)
    {
        auto& region = region_[i];
        if (region.type == USABLE_RAM_TYPE)
            continue;

        /* The overlap test is_reserved does, for the page at phy_addr. */
        if (phy_addr < (region.addr + region.len) &&
            region.addr < (phy_addr + consts::PAGE_SIZE))
        {
            ulong region_end = stdlib::round_up(region.addr + region.len,
                consts::PAGE_SIZE);
            if (region_end > end)
                end = region_end;
        }
    }

    /* Two reserved regions that touch are two calls: the caller re-asks at
       the end of the first and walks off the second. */
    return end;
}

ulong MemoryMap::get_next_reserved_start(ulong phy_addr, ulong limit)
{
    ulong next = limit;

    for (size_t i = 0; i < size_; i++)
    {
        auto& region = region_[i];
        if (region.type == USABLE_RAM_TYPE)
            continue;

        /* Rounded down: a region starting mid-page makes that whole page
           reserved, exactly as is_reserved would have said. */
        ulong start = stdlib::round_down(region.addr, consts::PAGE_SIZE);
        if (start > phy_addr && start < next)
            next = start;
    }

    return next;
}

ulong MemoryMap::get_usable_ram_bytes()
{
    ulong total = 0;
    for (size_t i = 0; i < size_; i++)
    {
        if (region_[i].type == USABLE_RAM_TYPE)
            total += region_[i].len;
    }

    return total;
}

ulong MemoryMap::get_usable_ram_end()
{
    ulong end = 0;
    for (size_t i = 0; i < size_; i++)
    {
        if (region_[i].type != USABLE_RAM_TYPE)
            continue;

        ulong region_end = region_[i].addr + region_[i].len;
        if (region_end > end)
            end = region_end;
    }

    return end;
}

bool MemoryMap::has_usable_ram_in(ulong start, ulong end)
{
    for (size_t i = 0; i < size_; i++)
    {
        if (region_[i].type != USABLE_RAM_TYPE)
            continue;

        if (region_[i].addr < end && (region_[i].addr + region_[i].len) > start)
            return true;
    }

    return false;
}

ulong MemoryMap::get_usable_ram_bytes_above(ulong limit)
{
    ulong total = 0;
    for (size_t i = 0; i < size_; i++)
    {
        if (region_[i].type != USABLE_RAM_TYPE)
            continue;

        ulong end = region_[i].addr + region_[i].len;
        if (end <= limit)
            continue;

        ulong start = (region_[i].addr < limit) ? limit : region_[i].addr;
        total += end - start;
    }

    return total;
}

}
}