#pragma once

#include <lib/stdlib.h>
#include <kernel/spin_lock.h>
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
};

static_assert(sizeof(Pte) == 8, "Invalid size");

struct PtePage final
{
    struct Pte Entry[512];
};

static_assert(sizeof(PtePage) == Const::PageSize, "Invalid size");

struct Page final
{
    void Init(ulong phyAddr)
    {
        ListEntry.Init();
        RefCount.Set(1);
        PhyAddr = phyAddr;
    }

    void Get()
    {
        RefCount.Inc();
    }

    void Put()
    {
        BugOn(RefCount.Get() == 0);
        RefCount.Dec();
    }

    ulong GetPhyAddress()
    {
        return PhyAddr;
    }

    Stdlib::ListEntry ListEntry;
    Kernel::Atomic RefCount;
    ulong PhyAddr;
};

static_assert(sizeof(Page) == 0x20, "Invalid size");
static_assert((Const::PageSize % sizeof(Page)) == 0, "Invalid size");

class BuiltinPageTable final
{
public:
    static BuiltinPageTable& GetInstance()
    {
        static BuiltinPageTable Instance;
        return Instance;
    }

    bool Setup();

    ulong VirtToPhys(ulong virtAddr);

    ulong PhysToVirt(ulong phyAddr);

    ulong GetRoot();

private:
    BuiltinPageTable(const BuiltinPageTable& other) = delete;
    BuiltinPageTable(BuiltinPageTable&& other) = delete;
    BuiltinPageTable& operator=(const BuiltinPageTable& other) = delete;
    BuiltinPageTable& operator=(BuiltinPageTable&& other) = delete;
    BuiltinPageTable();
    ~BuiltinPageTable();

    PtePage P4Page __attribute__((aligned(Const::PageSize)));
    PtePage P3KernelPage __attribute__((aligned(Const::PageSize)));
    PtePage P3UserPage __attribute__((aligned(Const::PageSize)));
    PtePage P2KernelPage[4] __attribute__((aligned(Const::PageSize)));
    PtePage P2UserPage[4] __attribute__((aligned(Const::PageSize)));
};

class PageTable final
{
public:
    static PageTable& GetInstance()
    {
        static PageTable Instance;
        return Instance;
    }

    bool Setup();
    bool SetupFreePagesList();

    ulong GetRoot();

    ulong TmpMapPage(ulong phyAddr);
    ulong TmpUnmapPage(ulong virtAddr);
    ulong TmpMapAddress(ulong phyAddr);

    ulong GetFreePagesCount();
    ulong GetTotalPagesCount();

    ulong GetVaEnd();

    Page* GetPage(ulong phyAddr);

    bool MapPage(ulong virtAddr, Page* page);
    Page* UnmapPage(ulong virtAddr);

    Page* AllocPage();
    void FreePage(Page* page);

private:
    PageTable(const PageTable& other) = delete;
    PageTable(PageTable&& other) = delete;
    PageTable& operator=(const PageTable& other) = delete;
    PageTable& operator=(PageTable&& other) = delete;
    PageTable();
    ~PageTable();

    ulong GetFreePage();
    ulong GetFreePageByTmpMap();

    bool SetupPage(ulong virtAddr, ulong phyAddr);

    bool GetFreePages();
    void ExcludeFreePages(ulong phyLimit);

    Page* AllocPageNoLock();

    ulong TmpMapStart;
    Kernel::SpinLock TmpMapLock;
    PtePage *TmpMapL1Page;

    static const size_t TmpMapPageCount = 512;
    Page *TmpMapPageArray[TmpMapPageCount];

    ulong GetL1Page(ulong virtAddr);

    ulong Root;

    SpinLock Lock;
    SpinLock FreePagesLock;
    ulong FreePages;

    Page* PageArray;
    ulong PageArrayCount;
    ulong HighestPhyAddr;
    Stdlib::ListEntry FreePagesList;
    ulong FreePagesCount;
    ulong TotalPagesCount;
};

}
}