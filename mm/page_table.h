#pragma once

#include <lib/stdlib.h>
#include <kernel/spin_lock.h>
#include <kernel/panic.h>

namespace Kernel
{

class PageTable final
{
public:
    static PageTable& GetInstance()
    {
        static PageTable Instance;
        return Instance;
    }

    bool Setup();

    ulong VirtToPhys(ulong virtAddr);

    ulong PhysToVirt(ulong phyAddr);

    ulong GetRoot();

    void UnmapNull();

private:
    PageTable(const PageTable& other) = delete;
    PageTable(PageTable&& other) = delete;
    PageTable& operator=(const PageTable& other) = delete;
    PageTable& operator=(PageTable&& other) = delete;
    PageTable();
    ~PageTable();

    bool MapPage(ulong virtAddr, ulong phyAddr);

    SpinLock Lock;

    struct Entry final
    {
        ulong Value;

        Entry()
            : Value(0)
        {
        }

        ulong Address()
        {
            return Value & (~((ulong)Shared::PageSize));
        }

        bool Present()
        {
            return (Value & (1 << PresentBit)) ? true : false;
        }

        void SetAddress(ulong address)
        {
            BugOn(address & (Shared::PageSize - 1));
            Value |= address;
        }

        void SetPresent()
        {
            Value |= (1 << PresentBit);
        }

        void SetCacheDisabled()
        {
            Value |= (1 << CacheDisabledBit);
        }

        void SetHuge()
        {
            Value |= (1 << HugeBit);
        }

        void SetWritable()
        {
            Value |= (1 << WritableBit);
        }

        void ClearPresent()
        {
            Value &= ~(1 << PresentBit);
        }

        static const ulong PresentBit = 0;
        static const ulong WritableBit = 1;
        static const ulong UserBit = 2;
        static const ulong WriteThrough = 3;
        static const ulong CacheDisabledBit = 4;
        static const ulong AccessedBit = 5;
        static const ulong DirtyBit = 6;
        static const ulong HugeBit = 7;
    };

    static_assert(sizeof(Entry) == 8, "Invalid size");

    struct Page final
    {
        struct Entry Entry[512];
    } __attribute__((aligned(Shared::PageSize)));

    Page P4Page;
    Page P3KernelPage;
    Page P3UserPage;
    Page P2KernelPage[4];
    Page P2UserPage[4];

    static_assert(sizeof(Page) == Shared::PageSize, "Invalid size");
};

}