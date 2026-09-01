#pragma once

#include <lib/stdlib.h>
#include <kernel/spin_lock.h>
#include <kernel/panic.h>
#include <hal/pte.h>

namespace Kernel
{

namespace Mm
{

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

    /* One past the highest physical address the bootstrap linear map
       reaches. Setup() sets it; it is the hard ceiling on what
       PageTable::GetFreePages can put on the free list, because building
       that list threads a next-pointer through the free pages themselves,
       which means writing to every one of them through this map. */
    ulong GetMappedLimit();

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

    ulong MappedLimit;
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

    static void InvalidateLocalTlb();
    static void InvalidateLocalTlbAddress(ulong virtAddr);
    static void InvalidateLocalTlbRange(ulong virtAddr, ulong count);

    ulong TmpMapPage(ulong phyAddr);
    ulong TmpUnmapPage(ulong virtAddr);
    ulong TmpMapAddress(ulong phyAddr);
    ulong TmpMapRange(ulong phyAddr, size_t len);

    ulong VirtToPhys(ulong virtAddr);
    ulong VirtToPhysLocked(ulong virtAddr);

    ulong GetFreePagesCount();
    ulong GetTotalPagesCount();

    ulong GetVaEnd();

    Page* GetPage(ulong phyAddr);

    bool MapPage(ulong virtAddr, Page* page);
    Page* UnmapPage(ulong virtAddr);

    /* Range forms of MapPage/UnmapPage. A run of consecutive VAs shares
       one walk down L4/L3/L2 and one temp mapping of the L1 table, so an
       n-page block costs 4 temp mappings instead of 4n; MapPage is the
       n == 1 case. Only the local TLB is invalidated, as with MapPage --
       the caller shoots down remote CPUs once for the whole range.

       MapPages takes an array of Page*, MapContiguousPages a run of Page
       structs, MapPhysPages the physical addresses of pages that already
       exist (it takes the mapping reference itself, like the others).
       All three are all-or-nothing: on failure nothing is left mapped and
       no mapping reference is left outstanding. */
    bool MapPages(ulong virtAddr, Page* const* pages, size_t count);
    bool MapContiguousPages(ulong virtAddr, Page* pages, size_t count);
    bool MapPhysPages(ulong virtAddr, const ulong* phyAddrs, size_t count);

    /* Drop the mapping reference on count pages; with freePages set they
       also go back to the free list (the AllocPage/MapPages pairing). */
    void UnmapPages(ulong virtAddr, size_t count, bool freePages);

    /* Cache policy for MapMmioRegion.

       MmioUncached is the only correct choice for device registers: every
       load and store reaches the device, in program order.

       MmioWriteCombining is for memory-like device RAM with no read side
       effects -- a framebuffer. Stores may be buffered, merged and
       reordered, which is what makes drawing affordable; the writer must
       call Hal::WcFlush() when the pixels have to be visible, and must
       never use it for registers. Falls back to uncached if the CPU cannot
       do write-combining (Hal::IsWriteCombiningAvailable). */
    enum MmioCachePolicy
    {
        MmioUncached = 0,
        MmioWriteCombining = 1,
    };

    /* Map a physical MMIO range into kernel virtual space.
       physAddr must be page-aligned.
       Returns kernel virtual address, or 0 on failure.

       The mapping is always non-executable: nothing is ever fetched from
       MMIO, and on real hardware a speculative fetch from device memory can
       trigger read side effects.

       Boot-ordering constraint: the mapping is placed at
       physAddr + KernelSpaceBase outside the VaAllocator, is permanent
       (never unmapped), and only the local TLB is invalidated. Callers
       must therefore run on the BSP before the APs are started
       (currently: HPET and driver init in Main2/BpStartup). */
    ulong MapMmioRegion(ulong physAddr, ulong sizeBytes,
        MmioCachePolicy policy = MmioUncached);

    /* Tighten permissions on an existing kernel-image mapping to enforce
       W^X: writable=false makes the range read-only, executable=false makes
       it non-executable (NX/PXN). Walks the 4KiB leaf PTEs in [virtAddr,
       virtAddr+sizeBytes) and invalidates the local TLB. */
    bool ProtectRange(ulong virtAddr, ulong sizeBytes, bool writable, bool executable);

    Page* AllocPage();
    static const ulong MaxContiguousPages = 128;
    Page* AllocContiguousPages(ulong count);
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
    void FreePageNoLock(Page* page);

    /* Where a range map takes its frames from: exactly one member is set.
       A tagged source keeps one range walk serving all three public forms
       -- this kernel has no lambdas to parameterize it with. */
    struct MapSource
    {
        Page* const* Ptrs;
        Page* Array;
        const ulong* PhyAddrs;
    };

    Page* SourcePage(const MapSource& src, size_t index);

    PtePage* WalkToL1Locked(ulong virtAddr, bool create);
    bool MapRangeLocked(ulong virtAddr, size_t count, const MapSource& src);
    void UnmapRangeLocked(ulong virtAddr, size_t count, bool freePages);

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
    /* Usable RAM below the PageArray limit, parked by ExcludeFreePages so
       Setup never allocates it; reclaimed in SetupFreePagesList. */
    ulong ExcludedPages;

    Page* PageArray;
    ulong PageArrayCount;
    ulong HighestPhyAddr;
    Stdlib::ListEntry FreePagesList;
    ulong FreePagesCount;
    ulong TotalPagesCount;
};

}
}