#pragma once

#include <lib/stdlib.h>
#include <kernel/panic.h>

namespace kernel
{

namespace mm
{

/* arm64 4K-granule descriptor, API-compatible with the x86 Pte (see
   arch/x86_64/pte.h — the accessor/helper names are the cross-arch
   contract used by mm/page_table.cpp):

   - SetPresent() marks a valid TABLE entry (levels 0-2) or PAGE entry
     (level 3): both need bit1 set. It also sets AF|SH(inner) — those bits
     are ignored on table descriptors, required on leaves.
   - SetHuge() converts the entry to a BLOCK descriptor (bit1 cleared),
     used for 2MiB mappings at the L2 level — same geometry as x86.
   - SetWritable() clears AP[2]: 0 (the default) is EL1-RW, so it matters
     only to undo SetReadOnly(), which PageTable::SetRangeProtection does
     (x86 always sets W too).
   - SetCacheDisabled() selects MAIR AttrIndx 1 = Device-nGnRE, clears SH
     (ignored for device memory) and sets PXN|UXN. Default AttrIndx 0 =
     Normal WB.
   - No hardware AF management: a valid entry without AF faults, so AF is
     always set together with the valid bit. */
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
        return value & ADDR_MASK;
    }

    bool present()
    {
        return (value & (1UL << PRESENT_BIT)) ? true : false;
    }

    bool huge()
    {
        return (value & HUGE_SW_BIT) ? true : false;
    }

    void set_address(ulong address)
    {
        BUG_ON(address & ~ADDR_MASK);
        value |= address;
    }

    /* Call-order independent w.r.t. SetHuge: the software "huge" marker
       (bit 55, ignored by the MMU) decides whether the descriptor is a
       table/page (bit1 set) or a block (bit1 clear). */
    void set_present()
    {
        value |= (1UL << PRESENT_BIT) | AF_BIT | SH_INNER;
        if (!(value & HUGE_SW_BIT))
            value |= (1UL << TABLE_BIT);
    }

    void set_cache_disabled()
    {
        value &= ~(ATTR_IDX_MASK | SH_INNER);
        /* Device memory is never executable: a speculative instruction
           fetch from MMIO can trigger read side effects on real hardware */
        value |= ATTR_IDX_DEVICE | PXN_BIT | UXN_BIT;
    }

    /* Write-combining equivalent: MAIR idx2 = Normal non-cacheable, so
       stores may be merged and reordered the way x86 WC does. Kept
       execute-never like device memory -- the only user is a framebuffer. */
    void set_write_combining()
    {
        value &= ~ATTR_IDX_MASK;
        value |= ATTR_IDX_NON_CACHEABLE | SH_INNER | PXN_BIT | UXN_BIT;
    }

    void set_huge()
    {
        value |= HUGE_SW_BIT;
        value &= ~(1UL << TABLE_BIT);
    }

    void set_writable()
    {
        /* AP[2]=0 is EL1 read-write: what a fresh entry already is, and what
           undoes SetReadOnly() */
        value &= ~AP_READ_ONLY;
    }

    void set_read_only()
    {
        value |= AP_READ_ONLY; /* AP[2] (bit 7) -> read-only at EL1 */
    }

    void set_no_execute()
    {
        value |= PXN_BIT | UXN_BIT; /* never executable */
    }

    void clear_no_execute()
    {
        /* Executable at EL1 only: UXN stays, there is no EL0 to run it */
        value &= ~PXN_BIT;
        value |= UXN_BIT;
    }

    void clear_present()
    {
        value &= ~(1UL << PRESENT_BIT);
    }

    void clear()
    {
        value = 0;
    }

    /* One index per translation level, top (L0 here, "L4" in the shared
       naming) to leaf. arm64 4K/48-bit uses the same 9-9-9-9-12 split as
       x86-64. */
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
    static const ulong TABLE_BIT = 1;

    static const ulong ATTR_IDX_MASK = 7UL << 2;
    static const ulong ATTR_IDX_DEVICE = 1UL << 2; /* MAIR idx1 = Device-nGnRE */
    static const ulong ATTR_IDX_NON_CACHEABLE = 2UL << 2; /* MAIR idx2 = Normal NC */
    static const ulong SH_INNER = 3UL << 8;
    static const ulong AF_BIT = 1UL << 10;
    static const ulong AP_READ_ONLY = 1UL << 7;    /* AP[2]: read-only */
    static const ulong PXN_BIT = 1UL << 53;       /* privileged execute-never */
    static const ulong UXN_BIT = 1UL << 54;       /* unprivileged execute-never */
    static const ulong HUGE_SW_BIT = 1UL << 55;    /* software bit: block entry */

    static const ulong ADDR_MASK = 0x0000FFFFFFFFF000UL;

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
