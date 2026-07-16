#pragma once

#include <lib/stdlib.h>
#include <kernel/panic.h>

namespace Kernel
{

namespace Mm
{

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
        ulong address =  Value & (~BitMask);
        BugOn(address & BitMask);
        return address;
    }

    bool Present()
    {
        return (Value & (1UL << PresentBit)) ? true : false;
    }

    bool Huge()
    {
        return (Value & (1UL << HugeBit)) ? true : false;
    }

    void SetAddress(ulong address)
    {
        BugOn(address & BitMask);
        Value |= address;
    }

    void SetPresent()
    {
        Value |= (1UL << PresentBit);
    }

    void SetCacheDisabled()
    {
        Value |= (1UL << CacheDisabledBit);
    }

    void SetHuge()
    {
        Value |= (1UL << HugeBit);
    }

    void SetWritable()
    {
        Value |= (1UL << WritableBit);
    }

    void ClearPresent()
    {
        Value &= ~(1UL << PresentBit);
    }

    void Clear()
    {
        Value = 0;
    }

    /* One index per translation level, top (L4) to leaf (L1).
       x86-64 4KiB paging: 9 bits per level, page offset 12 bits. */
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
    static const ulong WritableBit = 1;
    static const ulong UserBit = 2;
    static const ulong WriteThrough = 3;
    static const ulong CacheDisabledBit = 4;
    static const ulong AccessedBit = 5;
    static const ulong DirtyBit = 6;
    static const ulong HugeBit = 7;
    static const ulong MaxBit = 12;
    static const ulong BitMask = (1UL << MaxBit) - 1;

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
