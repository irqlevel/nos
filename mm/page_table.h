#pragma once

#include <lib/stdlib.h>
#include <kernel/spin_lock.h>
#include <kernel/panic.h>

namespace Kernel
{

namespace Mm
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

    bool Setup2();

private:
    PageTable(const PageTable& other) = delete;
    PageTable(PageTable&& other) = delete;
    PageTable& operator=(const PageTable& other) = delete;
    PageTable& operator=(PageTable&& other) = delete;
    PageTable();
    ~PageTable();

    bool MapPage(ulong virtAddr, ulong phyAddr);

    SpinLock Lock;

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

    static_assert(sizeof(Pte) == 8, "Invalid size");

    struct PtePage final
    {
        struct Pte Entry[512];
    };

    static_assert(sizeof(PtePage) == Shared::PageSize, "Invalid size");

    PtePage P4Page __attribute__((aligned(Shared::PageSize)));
    PtePage P3KernelPage __attribute__((aligned(Shared::PageSize)));
    PtePage P3UserPage __attribute__((aligned(Shared::PageSize)));
    PtePage P2KernelPage[4] __attribute__((aligned(Shared::PageSize)));
    PtePage P2UserPage[4] __attribute__((aligned(Shared::PageSize)));

    struct Page final
    {
        Shared::ListEntry ListEntry;
        Kernel::Atomic RefCounter;
        ulong Pfn;
    };

    static_assert(sizeof(Page) == 0x20, "Invalid size");

    struct Page *Pages;
    size_t PageCount;
    ulong State;

    Shared::ListEntry FreePagesList;
};

}
}