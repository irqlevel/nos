#pragma once

#include <lib/stdlib.h>
#include <kernel/panic.h>

namespace kernel
{

namespace mm
{

struct Pte final
{
    ulong value;

    Pte()
        : value(0)
    {
    }

    ~Pte()
    {
    }

    ulong address()
    {
        /* Bits 12-51 only: NX is bit 63, so an entry that maps a page
           non-executable would otherwise hand back an address with the top
           bit set -- and GetPage() a BUG -- the moment it is unmapped */
        return value & ADDR_MASK;
    }

    bool present()
    {
        return (value & (1UL << PRESENT_BIT)) ? true : false;
    }

    bool huge()
    {
        return (value & (1UL << HUGE_BIT)) ? true : false;
    }

    void set_address(ulong address)
    {
        BUG_ON(address & BIT_MASK);
        value |= address;
    }

    void set_present()
    {
        value |= (1UL << PRESENT_BIT);
    }

    void set_cache_disabled()
    {
        value |= (1UL << CACHE_DISABLED_BIT);
    }

    /* Write-combining: PAT=1, PCD=0, PWT=0 selects IA32_PAT entry 4, which
       Hal::SetupMemoryTypes() programs to WC. Only valid on a 4KiB leaf
       entry -- at L2/L3 the same bit 7 means "huge page" (HugeBit), and
       those levels carry the PAT bit at bit 12 instead. */
    void set_write_combining()
    {
        value &= ~((1UL << CACHE_DISABLED_BIT) | (1UL << WRITE_THROUGH));
        value |= (1UL << PAT_BIT);
    }

    void set_huge()
    {
        value |= (1UL << HUGE_BIT);
    }

    void set_writable()
    {
        value |= (1UL << WRITABLE_BIT);
    }

    void set_read_only()
    {
        value &= ~(1UL << WRITABLE_BIT);
    }

    void set_no_execute()
    {
        value |= NX_BIT; /* requires EFER.NXE; unused until x86 W^X lands */
    }

    void clear_no_execute()
    {
        value &= ~NX_BIT;
    }

    void clear_present()
    {
        value &= ~(1UL << PRESENT_BIT);
    }

    void clear()
    {
        value = 0;
    }

    /* One index per translation level, top (L4) to leaf (L1).
       x86-64 4KiB paging: 9 bits per level, page offset 12 bits. */
    static ulong l4_index(ulong virt_addr)
    {
        return (virt_addr >> (12 + 3 * 9)) & ((1 << 9) - 1);
    }

    static ulong l3_index(ulong virt_addr)
    {
        return (virt_addr >> (12 + 2 * 9)) & ((1 << 9) - 1);
    }

    static ulong l2_index(ulong virt_addr)
    {
        return (virt_addr >> (12 + 1 * 9)) & ((1 << 9) - 1);
    }

    static ulong l1_index(ulong virt_addr)
    {
        return (virt_addr >> (12 + 0 * 9)) & ((1 << 9) - 1);
    }

    static ulong huge_offset(ulong virt_addr)
    {
        return virt_addr & (HUGE_PAGE_SIZE - 1);
    }

    static const ulong PRESENT_BIT = 0;
    static const ulong WRITABLE_BIT = 1;
    static const ulong USER_BIT = 2;
    static const ulong WRITE_THROUGH = 3;
    static const ulong CACHE_DISABLED_BIT = 4;
    static const ulong ACCESSED_BIT = 5;
    static const ulong DIRTY_BIT = 6;
    static const ulong HUGE_BIT = 7;
    /* Same bit as HugeBit: it is the PAT selector on 4KiB leaf entries and
       the page-size bit on L2/L3 entries. */
    static const ulong PAT_BIT = 7;
    static const ulong MAX_BIT = 12;
    static const ulong BIT_MASK = (1UL << MAX_BIT) - 1;

    static const ulong NX_BIT = 1UL << 63;

    /* The frame address: bits 12 up to 51, the architectural limit on
       MAXPHYADDR. Above it sit bits the CPU ignores, the protection keys and
       NX, none of them part of the address. */
    static const ulong ADDR_MASK = 0x000FFFFFFFFFF000UL;

    static const ulong HUGE_PAGE_SHIFT = 21;
    static const ulong HUGE_PAGE_SIZE = 1UL << HUGE_PAGE_SHIFT;
};

static_assert(sizeof(Pte) == 8, "Invalid size");

struct PtePage final
{
    struct Pte entry[512];
};

static_assert(sizeof(PtePage) == consts::PAGE_SIZE, "Invalid size");

}
}
