#pragma once

#include <lib/stdlib.h>
#include <kernel/panic.h>

namespace Kernel
{

namespace Mm
{

/* arm64 4K-granule descriptor, API-compatible with the x86 Pte (see
   arch/x86_64/pte.h — the accessor/helper names are the cross-arch
   contract used by mm/page_table.cpp):

   - SetPresent() marks a valid TABLE entry (levels 0-2) or PAGE entry
     (level 3): both need bit1 set. It also sets AF|SH(inner) — those bits
     are ignored on table descriptors, required on leaves.
   - SetHuge() converts the entry to a BLOCK descriptor (bit1 cleared),
     used for 2MiB mappings at the L2 level — same geometry as x86.
   - SetWritable() is a no-op: AP[7]=0 (the default) is EL1-RW, and this
     kernel maps everything writable today (x86 also always sets W).
   - SetCacheDisabled() selects MAIR AttrIndx 1 = Device-nGnRE and clears
     SH (ignored for device memory). Default AttrIndx 0 = Normal WB.
   - No hardware AF management: a valid entry without AF faults, so AF is
     always set together with the valid bit. */
struct Pte final
{
    ulong Value;

    Pte()
        : Value(0)
    {
    }

    ~Pte()
    {
    }

    ulong Address()
    {
        return Value & AddrMask;
    }

    bool Present()
    {
        return (Value & (1UL << PresentBit)) ? true : false;
    }

    bool Huge()
    {
        return (Value & HugeSwBit) ? true : false;
    }

    void SetAddress(ulong address)
    {
        BugOn(address & ~AddrMask);
        Value |= address;
    }

    /* Call-order independent w.r.t. SetHuge: the software "huge" marker
       (bit 55, ignored by the MMU) decides whether the descriptor is a
       table/page (bit1 set) or a block (bit1 clear). */
    void SetPresent()
    {
        Value |= (1UL << PresentBit) | AfBit | ShInner;
        if (!(Value & HugeSwBit))
            Value |= (1UL << TableBit);
    }

    void SetCacheDisabled()
    {
        Value &= ~(AttrIdxMask | ShInner);
        Value |= AttrIdxDevice;
    }

    void SetHuge()
    {
        Value |= HugeSwBit;
        Value &= ~(1UL << TableBit);
    }

    void SetWritable()
    {
        /* AP[7]=0 (default) is already EL1 read-write */
    }

    void SetReadOnly()
    {
        Value |= ApReadOnly; /* AP[2] (bit 7) -> read-only at EL1 */
    }

    void SetNoExecute()
    {
        Value |= PxnBit | UxnBit; /* never executable */
    }

    void ClearPresent()
    {
        Value &= ~(1UL << PresentBit);
    }

    void Clear()
    {
        Value = 0;
    }

    /* One index per translation level, top (L0 here, "L4" in the shared
       naming) to leaf. arm64 4K/48-bit uses the same 9-9-9-9-12 split as
       x86-64. */
    static ulong L4Index(ulong virtAddr)
    {
        return (virtAddr >> (12 + 3 * 9)) & ((1 << 9) - 1);
    }

    static ulong L3Index(ulong virtAddr)
    {
        return (virtAddr >> (12 + 2 * 9)) & ((1 << 9) - 1);
    }

    static ulong L2Index(ulong virtAddr)
    {
        return (virtAddr >> (12 + 1 * 9)) & ((1 << 9) - 1);
    }

    static ulong L1Index(ulong virtAddr)
    {
        return (virtAddr >> (12 + 0 * 9)) & ((1 << 9) - 1);
    }

    static ulong HugeOffset(ulong virtAddr)
    {
        return virtAddr & (HugePageSize - 1);
    }

    static const ulong PresentBit = 0;
    static const ulong TableBit = 1;

    static const ulong AttrIdxMask = 7UL << 2;
    static const ulong AttrIdxDevice = 1UL << 2; /* MAIR idx1 = Device-nGnRE */
    static const ulong ShInner = 3UL << 8;
    static const ulong AfBit = 1UL << 10;
    static const ulong ApReadOnly = 1UL << 7;    /* AP[2]: read-only */
    static const ulong PxnBit = 1UL << 53;       /* privileged execute-never */
    static const ulong UxnBit = 1UL << 54;       /* unprivileged execute-never */
    static const ulong HugeSwBit = 1UL << 55;    /* software bit: block entry */

    static const ulong AddrMask = 0x0000FFFFFFFFF000UL;

    static const ulong HugePageShift = 21;
    static const ulong HugePageSize = 1UL << HugePageShift;
};

static_assert(sizeof(Pte) == 8, "Invalid size");

struct PtePage final
{
    struct Pte Entry[512];
};

static_assert(sizeof(PtePage) == Const::PageSize, "Invalid size");

}
}
