#include "page_table.h"
#include "memory_map.h"

#include <kernel/trace.h>
#include <hal/mmu.h>
#include <kernel/debug.h>
#include <kernel/preempt.h>

namespace Kernel
{

namespace Mm
{

BuiltinPageTable::BuiltinPageTable()
    : MappedLimit(0)
{
    Stdlib::MemSet(&P4Page, 0, sizeof(P4Page));

    Stdlib::MemSet(&P3KernelPage, 0, sizeof(P3KernelPage));
    Stdlib::MemSet(&P3UserPage, 0, sizeof(P3UserPage));

    Stdlib::MemSet(&P2KernelPage[0], 0, sizeof(P2KernelPage));
    Stdlib::MemSet(&P2UserPage[0], 0, sizeof(P2UserPage));

    Trace(0, "PageTable 0x%p P4Page 0x%p", this, &P4Page);
}

ulong BuiltinPageTable::VirtToPhys(ulong virtAddr)
{

    BugOn(virtAddr > MemoryMap::UserSpaceMax && virtAddr < MemoryMap::KernelSpaceBase);

    if (virtAddr <= MemoryMap::UserSpaceMax)
        return virtAddr;

    return virtAddr - MemoryMap::KernelSpaceBase;
}

ulong BuiltinPageTable::PhysToVirt(ulong phyAddr)
{

    return phyAddr + MemoryMap::KernelSpaceBase;
}

ulong BuiltinPageTable::GetRoot()
{
    return VirtToPhys((ulong)&P4Page);
}

ulong BuiltinPageTable::GetMappedLimit()
{
    return MappedLimit;
}

BuiltinPageTable::~BuiltinPageTable()
{
}

PageTable::PageTable()
    : Root(0)
    , ExcludedPages(0)
    , PageArray(nullptr)
    , PageArrayPhys(0)
    , PageArrayCount(0)
    , HighestPhyAddr(0)
    , FreePagesCount(0)
    , TotalPagesCount(0)
{
    Trace(0, "PageTable 0x%p", this);
    for (size_t i = 0; i < Stdlib::ArraySize(TmpMapPageArray); i++)
    {
        TmpMapPageArray[i] = nullptr;
    }
    TmpMapL1Page = nullptr;
}

PageTable::~PageTable()
{
}

bool PageTable::GetFreePages(ulong excludeLimit)
{
    auto& mmap = MemoryMap::GetInstance();

    /* Free-listing a page means writing a next-pointer into it, so the list
       can only hold what the bootstrap linear map reaches. RAM above that
       is real, reported by the firmware, and unusable to us. */
    const ulong BuiltinMapLimit = BuiltinPageTable::GetInstance().GetMappedLimit();
    BugOn(BuiltinMapLimit == 0);

    /* A 64 GiB machine spends seconds in this loop and seconds more in the
       drain, and to a log read after the fact silence and a hang look the
       same. One line per 8 GiB is eight of them on the largest machine this
       kernel can address and none at all on a small one. */
    const ulong ProgressPages = (8 * Const::GB) / Const::PageSize;
    const ulong usableMiB = mmap.GetUsableRamBytes() / Const::MB;
    ulong nextProgress = ProgressPages;

    /* Hoisted: the kernel image is one physical range, and comparing against
       it beat calling PhysToVirt twice per page. */
    const ulong kernelPhysStart = BuiltinPageTable::GetInstance().VirtToPhys(mmap.GetKernelStart());
    const ulong kernelPhysEnd = BuiltinPageTable::GetInstance().VirtToPhys(mmap.GetKernelEnd());

    for (size_t i = 0; i < mmap.GetRegionCount(); i++)
    {
        ulong addr, len, type;

        if (!mmap.GetRegion(i, addr, len, type))
            return false;

        if (type != MemoryMap::UsableRamType)
            continue;

        ulong memStart = Stdlib::RoundUp(addr, Const::PageSize);
        ulong memEnd = ((addr + len) / Const::PageSize) * Const::PageSize;

        if (memStart < Const::MB)
            memStart = Stdlib::RoundUp(Const::MB, Const::PageSize);

        if (memEnd > BuiltinMapLimit)
            memEnd = BuiltinMapLimit;

        Trace(PageAllocatorLL, "Phy memStart 0x%p memEnd 0x%p", memStart, memEnd);

        if (memStart >= memEnd)
            continue;

        Trace(PageAllocatorLL, "GetFreePages: region 0x%p-0x%p", memStart, memEnd);

        ulong address = memStart;
        while (address < memEnd)
        {
            /* Reserved carve-outs (the arm64 DTB, PageArray's backing, the
               firmware's own) overlap usable-RAM regions; free-listing them
               would clobber their contents. Skipped a run at a time rather
               than a page at a time -- there are a handful of boundaries and
               millions of pages. */
            ulong reservedEnd = mmap.GetReservedEnd(address);
            if (reservedEnd != 0)
            {
                if (reservedEnd > memEnd)
                    reservedEnd = memEnd;

                TotalPagesCount += (reservedEnd - address) / Const::PageSize;
                address = reservedEnd;
                continue;
            }

            ulong runEnd = mmap.GetNextReservedStart(address, memEnd);
            for (; address < runEnd; address += Const::PageSize)
            {
                TotalPagesCount++;

                if (TotalPagesCount >= nextProgress)
                {
                    Trace(0, "mm: free list, %u of %u MiB",
                        (TotalPagesCount * Const::PageSize) / Const::MB, usableMiB);
                    nextProgress += ProgressPages;
                }

                if (address >= kernelPhysStart && address < kernelPhysEnd)
                    continue;

                /* Pages whose identity address is shadowed by the kernel's
                   own VA window go on a side list rather than the main one:
                   they are ordinary usable RAM, but Setup reaches its
                   allocations through the bootstrap map, and these are
                   exactly the addresses that map to something else once the
                   real table is live. SetupFreePagesList hands them to the
                   runtime allocator (which only ever touches a page through
                   TmpMap) in a second pass, so nothing is leaked. Sorting
                   them here costs one comparison; it used to be a second walk
                   of the whole list, and the whole list is as long as the
                   machine has memory. */
                ulong* head = (address < excludeLimit) ? &ExcludedPages : &FreePages;

                *(ulong *)BuiltinPageTable::GetInstance().PhysToVirt(address) = *head;
                *head = address;
            }
        }
        Trace(PageAllocatorLL, "GetFreePages: inner loop done, total %u", TotalPagesCount);
    }

    Trace(PageAllocatorLL, "GetFreePages done, HighestPhyAddr 0x%p TotalPages %u",
        HighestPhyAddr, TotalPagesCount);

    /* Say plainly how much of the machine's RAM the kernel actually took.
       The per-region traces above print the clamped end, so a box with more
       memory than the bootstrap map covers looked, in the log, exactly like
       a box with 4GiB in it. */
    ulong usable = mmap.GetUsableRamBytes();
    ulong unreachable = mmap.GetUsableRamBytesAbove(BuiltinMapLimit);

    Trace(0, "mm: %u MiB usable RAM reported, %u MiB reachable (%u pages)",
        usable / Const::MB, (TotalPagesCount * Const::PageSize) / Const::MB,
        TotalPagesCount);

    if (unreachable != 0)
        Trace(0, "mm: %u MiB of it is above the bootstrap map at 0x%p and is not used",
            unreachable / Const::MB, BuiltinMapLimit);

    return true;
}

ulong PageTable::GetFreePage()
{
    if (FreePages == 0)
        return 0;

    ulong curr = FreePages;
    ulong next = *(ulong *)BuiltinPageTable::GetInstance().PhysToVirt(curr);
    FreePages = next;

    Stdlib::MemSet((void *)BuiltinPageTable::GetInstance().PhysToVirt(curr), 0, Const::PageSize);
    return curr;
}

void PageTable::DrainEarlyFreeList()
{
    /* The early list is threaded through the free pages themselves, so every
       step has to read one, and there are as many of them as the machine has
       RAM. Doing it with the general TmpMapPage/TmpUnmapPage pair costs two
       TLB invalidations, two lock round trips and two PageArray lookups per
       page; one slot held across the whole walk costs one of each. Safe
       because this runs on the BSP before any AP is started. */
    const ulong virtAddr = TmpMapStart;
    Pte* l1Entry = &TmpMapL1Page->Entry[Pte::L1Index(virtAddr)];

    BugOn(l1Entry->Present());

    /* Same eight-lines-on-the-biggest-machine budget as the scan. */
    const ulong ProgressPages = (8 * Const::GB) / Const::PageSize;
    ulong nextProgress = ((FreePagesCount / ProgressPages) + 1) * ProgressPages;

    while (FreePages != 0)
    {
        ulong phyAddr = FreePages;

        l1Entry->Value = 0;
        l1Entry->SetAddress(phyAddr);
        l1Entry->SetWritable();
        l1Entry->SetPresent();
        Hal::TlbFlushPage(virtAddr);

        FreePages = *(ulong *)virtAddr;

        /* Nothing is zeroed on the way to the list. AllocPageNoLock zeroes
           every page it hands out and FreePageNoLock does not, so a page
           sitting on the free list is not expected to be clean by anyone.
           Zeroing here was a write pass over every byte of RAM the machine
           has -- two thirds of the time this took, and all of it redone at
           the first allocation. */
        Page* page = GetPage(phyAddr);
        BugOn(!page);
        /* GetPage's +1 was only for the lookup; a page on the free list must
           sit at refcount 1 */
        page->Put();
        FreePagesList.InsertHead(&page->ListEntry);
        FreePagesCount++;

        if (FreePagesCount >= nextProgress)
        {
            Trace(0, "mm: %u MiB onto the free list",
                (FreePagesCount * Const::PageSize) / Const::MB);
            nextProgress += ProgressPages;
        }
    }

    l1Entry->Value = 0;
    Hal::TlbFlushPage(virtAddr);
}

ulong PageTable::GetL1Page(ulong virtAddr)
{
    BugOn(virtAddr & (Const::PageSize - 1));
    BugOn(!virtAddr);

    ulong l4Index = Pte::L4Index(virtAddr);
    ulong l3Index = Pte::L3Index(virtAddr);
    ulong l2Index = Pte::L2Index(virtAddr);

    if (!Root)
        return 0;

    PtePage* l4Page = (PtePage*)BuiltinPageTable::GetInstance().PhysToVirt(Root);
    Pte *l4Entry = &l4Page->Entry[l4Index];
    if (!l4Entry->Present())
        return 0;

    PtePage* l3Page = (PtePage*)BuiltinPageTable::GetInstance().PhysToVirt(l4Entry->Address());
    Pte *l3Entry = &l3Page->Entry[l3Index];
    if (!l3Entry->Present() || l3Entry->Huge())
        return 0;

    PtePage* l2Page = (PtePage*)BuiltinPageTable::GetInstance().PhysToVirt(l3Entry->Address());
    Pte *l2Entry = &l2Page->Entry[l2Index];
    if (!l2Entry->Present() || l2Entry->Huge())
        return 0;

    return l2Entry->Address();
}

ulong PageTable::VirtToPhys(ulong virtAddr)
{
    /* The walk below maps and unmaps shared TmpMap slots, and TmpUnmapPage
       only invalidates the local TLB. Disable preemption across the whole
       walk so a migration mid-walk cannot leave a stale mapping on the
       original CPU that a later slot reuse would mistranslate. */
    ulong flags = PreemptIrqSave();
    ulong phys = VirtToPhysLocked(virtAddr);
    PreemptIrqRestore(flags);
    return phys;
}

ulong PageTable::VirtToPhysLocked(ulong virtAddr)
{
    if (!Root)
        return 0;

    ulong l4Index = Pte::L4Index(virtAddr);
    ulong l3Index = Pte::L3Index(virtAddr);
    ulong l2Index = Pte::L2Index(virtAddr);
    ulong l1Index = Pte::L1Index(virtAddr);
    ulong offset  = virtAddr & (Const::PageSize - 1);

    PtePage* l4Page = (PtePage*)TmpMapPage(Root);
    if (!l4Page)
        return 0;
    Pte l4Entry = l4Page->Entry[l4Index];
    TmpUnmapPage((ulong)l4Page);
    if (!l4Entry.Present())
        return 0;

    PtePage* l3Page = (PtePage*)TmpMapPage(l4Entry.Address());
    if (!l3Page)
        return 0;
    Pte l3Entry = l3Page->Entry[l3Index];
    TmpUnmapPage((ulong)l3Page);
    if (!l3Entry.Present())
        return 0;

    /* 1GB block at the L3 level (the arm64 early device GiB); without this
       check the walk would treat the block's output address as an L2 table */
    if (l3Entry.Huge())
        return l3Entry.Address() + (virtAddr & (Const::GB - 1));

    PtePage* l2Page = (PtePage*)TmpMapPage(l3Entry.Address());
    if (!l2Page)
        return 0;
    Pte l2Entry = l2Page->Entry[l2Index];
    TmpUnmapPage((ulong)l2Page);
    if (!l2Entry.Present())
        return 0;

    /* 2MB huge page at L2 level */
    if (l2Entry.Huge())
    {
        ulong hugeOffset = Pte::HugeOffset(virtAddr);
        return l2Entry.Address() + hugeOffset;
    }

    PtePage* l1Page = (PtePage*)TmpMapPage(l2Entry.Address());
    if (!l1Page)
        return 0;
    Pte l1Entry = l1Page->Entry[l1Index];
    TmpUnmapPage((ulong)l1Page);
    if (!l1Entry.Present())
        return 0;

    return l1Entry.Address() + offset;
}

bool PageTable::SetupPage(ulong virtAddr, ulong phyAddr)
{
    BugOn(virtAddr & (Const::PageSize - 1));
    BugOn(phyAddr & (Const::PageSize - 1));

    ulong l4Index = Pte::L4Index(virtAddr);
    ulong l3Index = Pte::L3Index(virtAddr);
    ulong l2Index = Pte::L2Index(virtAddr);
    ulong l1Index = Pte::L1Index(virtAddr);

    if (Root == 0)
    {
        Root = GetFreePage();
        if (!Root)
            return false;
        Trace(0, "Root 0x%p", Root);
    }

    PtePage* l4Page = (PtePage*)BuiltinPageTable::GetInstance().PhysToVirt(Root);
    Pte *l4Entry = &l4Page->Entry[l4Index];
    if (!l4Entry->Present()) {
        ulong addr = GetFreePage();
        if (addr == 0)
            return false;

        l4Entry->SetAddress(addr);
        l4Entry->SetWritable();
        l4Entry->SetPresent();
    }

    PtePage* l3Page = (PtePage*)BuiltinPageTable::GetInstance().PhysToVirt(l4Entry->Address());
    Pte *l3Entry = &l3Page->Entry[l3Index];
    if (!l3Entry->Present()) {
        ulong addr = GetFreePage();
        if (addr == 0)
            return false;

        l3Entry->SetAddress(addr);
        l3Entry->SetWritable();
        l3Entry->SetPresent();
    }

    PtePage* l2Page = (PtePage*)BuiltinPageTable::GetInstance().PhysToVirt(l3Entry->Address());
    Pte *l2Entry = &l2Page->Entry[l2Index];
    if (!l2Entry->Present()) {
        ulong addr = GetFreePage();
        if (addr == 0)
            return false;

        l2Entry->SetAddress(addr);
        l2Entry->SetWritable();
        l2Entry->SetPresent();
    }

    PtePage* l1Page = (PtePage*)BuiltinPageTable::GetInstance().PhysToVirt(l2Entry->Address());
    Pte *l1Entry = &l1Page->Entry[l1Index];

    Trace(5, "va 0x%p pha 0x%p l4 %u(0x%p) l3 %u(0x%p) l2 %u(0x%p) l1 %u(0x%p)",
        virtAddr, phyAddr, l4Index, l4Entry, l3Index, l3Entry, l2Index, l2Entry, l1Index, l1Entry);

    if (l1Entry->Present())
        return false;

    if (phyAddr)
    {
        l1Entry->SetAddress(phyAddr);
        l1Entry->SetWritable();
        l1Entry->SetPresent();
    } else {
        l1Entry->Clear();
    }
    Hal::TlbFlushPage(virtAddr);
    return true;
}

bool PageTable::SetupHugePage(ulong virtAddr, ulong phyAddr)
{
    BugOn(virtAddr & (HugePageSize - 1));
    BugOn(phyAddr & (HugePageSize - 1));
    BugOn(Root == 0);

    auto& bpt = BuiltinPageTable::GetInstance();

    PtePage* l4Page = (PtePage*)bpt.PhysToVirt(Root);
    Pte* l4Entry = &l4Page->Entry[Pte::L4Index(virtAddr)];
    if (!l4Entry->Present())
    {
        ulong addr = GetFreePage();
        if (addr == 0)
            return false;

        l4Entry->SetAddress(addr);
        l4Entry->SetWritable();
        l4Entry->SetPresent();
    }

    PtePage* l3Page = (PtePage*)bpt.PhysToVirt(l4Entry->Address());
    Pte* l3Entry = &l3Page->Entry[Pte::L3Index(virtAddr)];
    if (!l3Entry->Present())
    {
        ulong addr = GetFreePage();
        if (addr == 0)
            return false;

        l3Entry->SetAddress(addr);
        l3Entry->SetWritable();
        l3Entry->SetPresent();
    }

    /* A 1GiB block here instead of a table would swallow everything else
       living in this GiB of kernel VA. */
    if (l3Entry->Huge())
        return false;

    PtePage* l2Page = (PtePage*)bpt.PhysToVirt(l3Entry->Address());
    Pte* l2Entry = &l2Page->Entry[Pte::L2Index(virtAddr)];
    if (l2Entry->Present())
        return false;

    /* The permissions SetupPage's 4KiB leaf gets, no more and no less. */
    l2Entry->SetAddress(phyAddr);
    l2Entry->SetWritable();
    l2Entry->SetHuge();
    l2Entry->SetPresent();
    Hal::TlbFlushPage(virtAddr);
    return true;
}

ulong PageTable::ReservePageArray(ulong bytes, ulong lowerBound)
{
    auto& mmap = MemoryMap::GetInstance();
    auto& bpt = BuiltinPageTable::GetInstance();

    /* It has to be writable through the bootstrap map: Setup fills PageArray
       in through PhysToVirt, while the real table is not live yet. */
    const ulong mapLimit = bpt.GetMappedLimit();

    const ulong kernelStart = bpt.VirtToPhys(mmap.GetKernelStart());
    const ulong kernelEnd = bpt.VirtToPhys(mmap.GetKernelEnd());

    for (size_t i = 0; i < mmap.GetRegionCount(); i++)
    {
        ulong addr, len, type;
        if (!mmap.GetRegion(i, addr, len, type))
            break;

        if (type != MemoryMap::UsableRamType)
            continue;

        ulong start = Stdlib::RoundUp((addr < lowerBound) ? lowerBound : addr,
            HugePageSize);

        ulong end = addr + len;
        if (end > mapLimit)
            end = mapLimit;

        if (start >= end || (end - start) < bytes)
            continue;

        for (ulong cand = start; (cand + bytes) <= end; cand += HugePageSize)
        {
            /* The kernel image is plain usable RAM as far as the firmware is
               concerned; nobody marks it reserved. */
            if (cand < kernelEnd && (cand + bytes) > kernelStart)
                continue;

            if (mmap.IsReserved(cand, bytes))
                continue;

            return cand;
        }
    }

    return 0;
}

bool PageTable::ProtectRange(ulong virtAddr, ulong sizeBytes, bool writable,
    bool executable)
{
    Stdlib::AutoLock lock(Lock);

    if (Root == 0)
        return false;

    BugOn(virtAddr & (Const::PageSize - 1));

    ulong end = virtAddr + Stdlib::RoundUp(sizeBytes, Const::PageSize);

    /* Runs after the real page table is active, so the page-table pages are
       reached through the TmpMap window (the builtin linear map is gone),
       exactly like MapPage/UnmapPage. */
    for (ulong va = virtAddr; va < end; va += Const::PageSize)
    {
        PtePage* l4 = (PtePage*)TmpMapPage(Root);
        Pte* l4e = &l4->Entry[Pte::L4Index(va)];
        if (!l4e->Present()) { TmpUnmapPage((ulong)l4); return false; }
        ulong l3phys = l4e->Address();
        TmpUnmapPage((ulong)l4);

        PtePage* l3 = (PtePage*)TmpMapPage(l3phys);
        Pte* l3e = &l3->Entry[Pte::L3Index(va)];
        if (!l3e->Present() || l3e->Huge())
        {
            TmpUnmapPage((ulong)l3); /* kernel image is 4KiB-mapped */
            return false;
        }
        ulong l2phys = l3e->Address();
        TmpUnmapPage((ulong)l3);

        PtePage* l2 = (PtePage*)TmpMapPage(l2phys);
        Pte* l2e = &l2->Entry[Pte::L2Index(va)];
        if (!l2e->Present() || l2e->Huge())
        {
            TmpUnmapPage((ulong)l2); /* kernel image is 4KiB-mapped */
            return false;
        }
        ulong l1phys = l2e->Address();
        TmpUnmapPage((ulong)l2);

        PtePage* l1 = (PtePage*)TmpMapPage(l1phys);
        Pte* l1e = &l1->Entry[Pte::L1Index(va)];
        if (!l1e->Present()) { TmpUnmapPage((ulong)l1); return false; }

        if (!writable)
            l1e->SetReadOnly();
        if (!executable)
            l1e->SetNoExecute();
        TmpUnmapPage((ulong)l1);

        InvalidateLocalTlbAddress(va);
    }

    return true;
}

Page* PageTable::GetPage(ulong phyAddr)
{
    BugOn(phyAddr & (Const::PageSize - 1));
    ulong index = phyAddr / Const::PageSize;

    BugOn(index >= PageArrayCount);
    Page* page = &PageArray[index];
    BugOn(page->GetPhyAddress() != phyAddr);
    page->Get();
    return page;
}

bool PageTable::Setup()
{
    Trace(0, "PageTable setup");

    auto& mmap = MemoryMap::GetInstance();
    auto& bpt = BuiltinPageTable::GetInstance();

    /* PageArray has to be sized and placed before the free list is built --
       its home is carved out of the map so the list never sees those pages --
       so the top of usable memory is computed here rather than picked up
       from GetFreePages. Same clamp GetFreePages applies per region: the
       list can only hold what the bootstrap map reaches. */
    HighestPhyAddr = Stdlib::RoundUp(mmap.GetUsableRamEnd(), Const::PageSize);
    if (HighestPhyAddr > bpt.GetMappedLimit())
        HighestPhyAddr = bpt.GetMappedLimit();

    TmpMapL1Page = (PtePage *)mmap.GetKernelEnd();
    TmpMapStart = Stdlib::RoundUp(mmap.GetKernelEnd() + Const::PageSize, HugePageSize);
    PageArray = (Page*)(TmpMapStart + Stdlib::ArraySize(TmpMapPageArray) * Const::PageSize);
    BugOn((ulong)PageArray & (HugePageSize - 1));

    ulong pageArrayCount = HighestPhyAddr / Const::PageSize + 1;
    ulong pageArrayBytes = Stdlib::RoundUp(pageArrayCount * sizeof(Page), HugePageSize);
    ulong pageArrayLimit = (ulong)PageArray + pageArrayBytes;

    /* Above the VA window's own physical shadow, so the carve-out is nowhere
       near the pages ExcludeFreePages parks. */
    PageArrayPhys = ReservePageArray(pageArrayBytes,
        Stdlib::RoundUp(bpt.VirtToPhys(pageArrayLimit), HugePageSize));
    if (PageArrayPhys == 0)
    {
        Trace(0, "mm: no %u MiB contiguous window for PageArray",
            pageArrayBytes / Const::MB);
        return false;
    }

    /* Reserving it in the map is the whole mechanism: GetFreePages already
       skips any page a reserved region covers. */
    if (!mmap.AddRegion(PageArrayPhys, pageArrayBytes, MemoryMap::ReservedType))
    {
        Trace(0, "mm: no room in the memory map to reserve PageArray");
        return false;
    }

    Trace(0, "mm: PageArray %u MiB at phys 0x%p, %u huge pages",
        pageArrayBytes / Const::MB, PageArrayPhys, pageArrayBytes / HugePageSize);

    if (!GetFreePages(bpt.VirtToPhys(pageArrayLimit)))
        return false;

    for (ulong address = mmap.GetKernelStart(); address < mmap.GetKernelEnd(); address+= Const::PageSize)
    {
        if (!SetupPage(address, BuiltinPageTable::GetInstance().VirtToPhys(address)))
        {
            Trace(0, "can't setup page");
            return false;
        }
    }

    Trace(0, "TmpMapStart 0x%p", TmpMapStart);
    for (size_t i = 0; i < Stdlib::ArraySize(TmpMapPageArray); i++)
    {
        if (!SetupPage(TmpMapStart + i * Const::PageSize, 0))
            return false;

        TmpMapPageArray[i] = nullptr;
    }

    auto tmpMapL1PagePhyAddr = GetL1Page(TmpMapStart);
    for (size_t i = 0; i < Stdlib::ArraySize(TmpMapPageArray); i++)
        BugOn(tmpMapL1PagePhyAddr != GetL1Page(TmpMapStart + i * Const::PageSize));

    if (!SetupPage((ulong)TmpMapL1Page, tmpMapL1PagePhyAddr))
        return false;

    if (!SetupPage(0, 0))
        return false;

    Trace(0, "PageArray setup, highestPhyAddr 0x%p", HighestPhyAddr);

    Trace(0, "PageArray setup, pageArray 0x%p pageArrayLimit 0x%p", PageArray, pageArrayLimit);

    for (ulong offset = 0; offset < pageArrayBytes; offset += HugePageSize)
    {
        if (!SetupHugePage((ulong)PageArray + offset, PageArrayPhys + offset))
        {
            Trace(0, "mm: can't map PageArray at 0x%p", (ulong)PageArray + offset);
            return false;
        }
    }

    /* Filled through the bootstrap map rather than through PageArray's own
       virtual address: the real table is not live yet, and the backing is
       one contiguous physical run, so this is a straight linear write. */
    Page* pages = (Page*)bpt.PhysToVirt(PageArrayPhys);
    for (ulong i = 0; i < pageArrayCount; i++)
        pages[i].Init(i * Const::PageSize);

    PageArrayCount = pageArrayCount;

    Trace(0, "PageArray setup done, count %u", PageArrayCount);
    return true;
}

bool PageTable::SetupFreePagesList()
{
    FreePagesCount = 0;

    /* Pass 1 drains the early free list; pass 2 drains the pages Setup
       parked below the PageArray limit (plain usable RAM, safe now that all
       page access goes through TmpMap). */
    for (ulong pass = 0; pass < 2; pass++)
    {
        DrainEarlyFreeList();

        FreePages = ExcludedPages;
        ExcludedPages = 0;
    }

    Trace(0, "FreePagesCount %u", FreePagesCount);
    return true;
}

Page* PageTable::AllocPage()
{
    Stdlib::AutoLock lock(Lock);

    return AllocPageNoLock();
}

Page* PageTable::AllocPageNoLock()
{
    if (FreePagesList.IsEmpty())
    {
        return nullptr;
    }

    //DebugWait();
    Page* page = CONTAINING_RECORD(FreePagesList.RemoveHead(), Page, ListEntry);
    page->ListEntry.Init(); /* Self-pointing = not on any list */
    ulong va = TmpMapPage(page->GetPhyAddress());
    BugOn(!va);
    Stdlib::MemSet((void*)va, 0, Const::PageSize);
    TmpUnmapPage(va);
    FreePagesCount--;
    return page;
}

Page* PageTable::AllocContiguousPages(ulong count)
{
    Stdlib::AutoLock lock(Lock);

    if (count == 0 || count > MaxContiguousPages || FreePagesList.IsEmpty())
        return nullptr;

    /* Walk the free list and for each free page, check if the next
       count-1 pages in the PageArray are also on the free list and
       physically contiguous.  A page is on the free list if its
       ListEntry is NOT self-pointing (AllocPageNoLock re-inits the
       ListEntry to self-pointing upon removal). */
    auto* entry = FreePagesList.Flink;
    while (entry != &FreePagesList)
    {
        Page* first = CONTAINING_RECORD(entry, Page, ListEntry);
        ulong basePhyAddr = first->GetPhyAddress();
        ulong baseIndex = (ulong)(first - PageArray);

        if (baseIndex + count > PageArrayCount)
        {
            entry = entry->Flink;
            continue;
        }

        bool ok = true;
        for (ulong j = 0; j < count; j++)
        {
            Page* p = &PageArray[baseIndex + j];
            if (p->GetPhyAddress() != basePhyAddr + j * Const::PageSize)
            {
                ok = false;
                break;
            }
            /* Self-pointing ListEntry means the page is NOT on the free list. */
            if (p->ListEntry.Flink == &p->ListEntry)
            {
                ok = false;
                break;
            }
        }

        if (!ok)
        {
            entry = entry->Flink;
            continue;
        }

        /* Found a contiguous run.  Remove all pages from the free list
           and zero them. */
        for (ulong j = 0; j < count; j++)
        {
            Page* p = &PageArray[baseIndex + j];
            p->ListEntry.RemoveInit();
            FreePagesCount--;
            ulong va = TmpMapPage(p->GetPhyAddress());
            BugOn(!va);
            Stdlib::MemSet((void*)va, 0, Const::PageSize);
            TmpUnmapPage(va);
        }

        return first;
    }

    return nullptr;
}

void PageTable::FreePageNoLock(Page* page)
{
    FreePagesCount++;
    FreePagesList.InsertHead(&page->ListEntry);
}

void PageTable::FreePage(Page* page)
{
    Stdlib::AutoLock lock(Lock);

    FreePageNoLock(page);
}

/* Sentinel for TmpMapPageArray when page was mapped without a Page struct (e.g. reserved ACPI region). */
static Page* const TmpMapDirectSentinel = (Page*)1;

ulong PageTable::TmpMapPage(ulong phyAddr)
{
    Stdlib::AutoLock lock(TmpMapLock);

    BugOn(phyAddr & (Const::PageSize - 1));

    bool havePage = (phyAddr / Const::PageSize) < PageArrayCount;

    for (size_t i = 0; i < Stdlib::ArraySize(TmpMapPageArray); i++)
    {
        if (!TmpMapPageArray[i]) {
            ulong virtAddr = TmpMapStart + i * Const::PageSize;
            ulong l1Index = Pte::L1Index(virtAddr);
            Pte *l1Entry = &TmpMapL1Page->Entry[l1Index];
            if (l1Entry->Present())
                return 0;

            l1Entry->SetAddress(phyAddr);
            /* TmpMap serves both RAM (free pages, ACPI tables) and MMIO
               (LAPIC, IOAPIC): only non-RAM gets mapped uncached. */
            if (!MemoryMap::GetInstance().IsUsableRam(phyAddr))
                l1Entry->SetCacheDisabled();
            l1Entry->SetWritable();
            l1Entry->SetPresent();
            Hal::TlbFlushPage(virtAddr);

            if (havePage) {
                Page* page = GetPage(phyAddr);
                BugOn(!page);
                TmpMapPageArray[i] = page;
            } else {
                /* Map reserved physical range (e.g. ACPI tables) without a Page struct. */
                TmpMapPageArray[i] = TmpMapDirectSentinel;
            }
            return virtAddr;
        }
    }

    return 0;
}

ulong PageTable::TmpUnmapPage(ulong virtAddr)
{
    Stdlib::AutoLock lock(TmpMapLock);

    BugOn(virtAddr & (Const::PageSize - 1));
    BugOn(!virtAddr);
    BugOn(virtAddr < TmpMapStart);
    BugOn(virtAddr >= (TmpMapStart + Stdlib::ArraySize(TmpMapPageArray) * Const::PageSize));
    size_t i = (virtAddr - TmpMapStart) / Const::PageSize;
    Page* page = TmpMapPageArray[i];
    BugOn(!page);

    ulong l1Index = Pte::L1Index(virtAddr);
    Pte *l1Entry = &TmpMapL1Page->Entry[l1Index];
    BugOn(!l1Entry->Present());
    auto phyAddr = l1Entry->Address();
    l1Entry->Clear();
    Hal::TlbFlushPage(virtAddr);
    TmpMapPageArray[i] = nullptr;
    if (page != TmpMapDirectSentinel) {
        BugOn(page->GetPhyAddress() != phyAddr);
        page->Put();
    }

    return phyAddr;
}

ulong PageTable::GetRoot()
{
    /*
     * Root is set once in Setup() before any AP starts
     * and never changes — no lock needed.  Taking the
     * SpinLock here is unsafe for APs that call GetRoot()
     * before SetCr3: SpinLock::Lock() invokes GetBootTime()
     * which reads kvmclock PvClock memory through a VA that
     * only resolves correctly under the kernel PageTable,
     * not the boot page tables the AP still uses.
     */
    return Root;
}

void PageTable::InvalidateLocalTlb()
{
    Hal::TlbFlushAll();
}

void PageTable::InvalidateLocalTlbAddress(ulong virtAddr)
{
    Hal::TlbFlushPage(virtAddr);
}

void PageTable::InvalidateLocalTlbRange(ulong virtAddr, ulong count)
{
    for (ulong i = 0; i < count; i++)
        Hal::TlbFlushPage(virtAddr + i * Const::PageSize);
}

ulong PageTable::TmpMapAddress(ulong phyAddr)
{
    ulong phyPage = phyAddr & ~(Const::PageSize - 1);
    ulong vaPage = TmpMapPage(phyPage);
    if (!vaPage)
        return 0;

    return vaPage + (phyAddr - phyPage);
}

ulong PageTable::TmpMapRange(ulong phyAddr, size_t len)
{
    ulong phyBase = phyAddr & ~(Const::PageSize - 1);
    ulong offset = phyAddr - phyBase;
    size_t pageCount = (offset + len + Const::PageSize - 1) / Const::PageSize;

    if (pageCount <= 1)
        return TmpMapAddress(phyAddr);

    Stdlib::AutoLock lock(TmpMapLock);

    for (size_t i = 0; i + pageCount <= Stdlib::ArraySize(TmpMapPageArray); i++)
    {
        bool allFree = true;
        for (size_t j = 0; j < pageCount; j++)
        {
            if (TmpMapPageArray[i + j])
            {
                allFree = false;
                i += j; /* skip to after the occupied slot */
                break;
            }
        }
        if (!allFree)
            continue;

        for (size_t j = 0; j < pageCount; j++)
        {
            ulong thisPhyPage = phyBase + j * Const::PageSize;
            ulong virtAddr = TmpMapStart + (i + j) * Const::PageSize;
            ulong l1Index = Pte::L1Index(virtAddr);
            Pte *l1Entry = &TmpMapL1Page->Entry[l1Index];
            if (l1Entry->Present())
                return 0;

            l1Entry->SetAddress(thisPhyPage);
            if (!MemoryMap::GetInstance().IsUsableRam(thisPhyPage))
                l1Entry->SetCacheDisabled();
            l1Entry->SetWritable();
            l1Entry->SetPresent();
            Hal::TlbFlushPage(virtAddr);

            bool havePage = (thisPhyPage / Const::PageSize) < PageArrayCount;
            if (havePage) {
                Page* page = GetPage(thisPhyPage);
                BugOn(!page);
                TmpMapPageArray[i + j] = page;
            } else {
                TmpMapPageArray[i + j] = TmpMapDirectSentinel;
            }
        }
        return TmpMapStart + i * Const::PageSize + offset;
    }

    return 0;
}

Page* PageTable::SourcePage(const MapSource& src, size_t index)
{
    if (src.Ptrs != nullptr)
        return src.Ptrs[index];

    if (src.Array != nullptr)
        return &src.Array[index];

    /* GetPage's reference is only for the lookup; MapRangeLocked takes the
       mapping reference of its own and releases this one. */
    return GetPage(src.PhyAddrs[index]);
}

/* Walk down to the L1 table that maps virtAddr and return it temp-mapped,
   or nullptr. With create set, a missing L4/L3/L2 entry is filled in with
   a fresh table; without it a missing level means the caller asked about a
   VA that was never mapped, which is a bug in the caller. Lock must be
   held, and the caller TmpUnmapPage()s the result. */
PtePage* PageTable::WalkToL1Locked(ulong virtAddr, bool create)
{
    /* No BugOn: MapPage answered a missing Root with a plain failure, and
       a walk can also fail on a legitimately exhausted TmpMap. */
    if (Root == 0)
        return nullptr;

    PtePage* table = (PtePage*)TmpMapPage(Root);
    if (table == nullptr)
        return nullptr;

    /* L4 -> L3 -> L2; each step consumes that level's slice of the VA and
       leaves the next table temp-mapped in its place. */
    for (ulong level = 4; level > 1; level--)
    {
        ulong index = (level == 4) ? Pte::L4Index(virtAddr)
                    : (level == 3) ? Pte::L3Index(virtAddr)
                                   : Pte::L2Index(virtAddr);

        Pte* entry = &table->Entry[index];
        if (!entry->Present())
        {
            if (!create)
            {
                BugOn(1);
                TmpUnmapPage((ulong)table);
                return nullptr;
            }

            Page* page = AllocPageNoLock();
            if (page == nullptr)
            {
                TmpUnmapPage((ulong)table);
                return nullptr;
            }

            entry->SetAddress(page->GetPhyAddress());
            entry->SetWritable();
            entry->SetPresent();
        }

        PtePage* next = (PtePage*)TmpMapPage(entry->Address());
        TmpUnmapPage((ulong)table);
        if (next == nullptr)
            return nullptr;

        table = next;
    }

    return table;
}

/* The one range mapper behind MapPage and the three MapXxxPages forms.
   The walk is redone only when the run steps into the next L1 table, which
   for a naturally aligned block of at most MaxContiguousPages pages never
   happens. Lock must be held. */
bool PageTable::MapRangeLocked(ulong virtAddr, size_t count, const MapSource& src)
{
    BugOn(virtAddr & (Const::PageSize - 1));

    PtePage* l1Page = nullptr;
    size_t mapped = 0;

    for (size_t i = 0; i < count; i++)
    {
        ulong va = virtAddr + i * Const::PageSize;
        ulong l1Index = Pte::L1Index(va);

        BugOn(va >= TmpMapStart && va < (TmpMapStart + Stdlib::ArraySize(TmpMapPageArray) * Const::PageSize));

        if (l1Page == nullptr)
        {
            l1Page = WalkToL1Locked(va, true);
            if (l1Page == nullptr)
                break;
        }

        Pte* l1Entry = &l1Page->Entry[l1Index];
        if (l1Entry->Present())
            break;

        Page* page = SourcePage(src, i);
        if (page != nullptr)
        {
            page->Get();
            l1Entry->SetAddress(page->GetPhyAddress());
            l1Entry->SetWritable();
            l1Entry->SetPresent();
            if (src.PhyAddrs != nullptr)
                page->Put(); /* balance SourcePage's GetPage */
        }
        else
        {
            l1Entry->Clear();
        }

        Hal::TlbFlushPage(va);
        mapped++;

        /* Last entry of this table: the next VA starts a new walk. */
        if (l1Index == (Stdlib::ArraySize(l1Page->Entry) - 1))
        {
            TmpUnmapPage((ulong)l1Page);
            l1Page = nullptr;
        }
    }

    if (l1Page != nullptr)
        TmpUnmapPage((ulong)l1Page);

    if (mapped == count)
        return true;

    /* All-or-nothing: undo the prefix that did get mapped, leaving the
       pages themselves to the caller. */
    if (mapped != 0)
        UnmapRangeLocked(virtAddr, mapped, false);

    return false;
}

/* Range twin of MapRangeLocked. A missing entry is a caller bug (BugOn),
   but the walk carries on so the rest of the range is still torn down.
   Lock must be held. */
void PageTable::UnmapRangeLocked(ulong virtAddr, size_t count, bool freePages)
{
    BugOn(virtAddr & (Const::PageSize - 1));

    PtePage* l1Page = nullptr;

    for (size_t i = 0; i < count; i++)
    {
        ulong va = virtAddr + i * Const::PageSize;
        ulong l1Index = Pte::L1Index(va);

        BugOn(va >= TmpMapStart && va < (TmpMapStart + Stdlib::ArraySize(TmpMapPageArray) * Const::PageSize));

        if (l1Page == nullptr)
        {
            l1Page = WalkToL1Locked(va, false);
            if (l1Page == nullptr)
                return;
        }

        Pte* l1Entry = &l1Page->Entry[l1Index];
        if (!l1Entry->Present())
        {
            BugOn(1);
        }
        else
        {
            ulong phyAddr = l1Entry->Address();
            l1Entry->Clear();
            BugOn(!phyAddr);
            Hal::TlbFlushPage(va);

            Page* page = GetPage(phyAddr);
            page->Put(); /* balance GetPage's lookup reference */
            if (freePages)
                FreePageNoLock(page);
            page->Put(); /* the mapping reference MapRangeLocked took */
        }

        if (l1Index == (Stdlib::ArraySize(l1Page->Entry) - 1))
        {
            TmpUnmapPage((ulong)l1Page);
            l1Page = nullptr;
        }
    }

    if (l1Page != nullptr)
        TmpUnmapPage((ulong)l1Page);
}

bool PageTable::MapPage(ulong virtAddr, Page* page)
{
    Stdlib::AutoLock lock(Lock);

    MapSource src = { &page, nullptr, nullptr };

    return MapRangeLocked(virtAddr, 1, src);
}

bool PageTable::MapPages(ulong virtAddr, Page* const* pages, size_t count)
{
    Stdlib::AutoLock lock(Lock);

    MapSource src = { pages, nullptr, nullptr };

    return MapRangeLocked(virtAddr, count, src);
}

bool PageTable::MapContiguousPages(ulong virtAddr, Page* pages, size_t count)
{
    Stdlib::AutoLock lock(Lock);

    MapSource src = { nullptr, pages, nullptr };

    return MapRangeLocked(virtAddr, count, src);
}

bool PageTable::MapPhysPages(ulong virtAddr, const ulong* phyAddrs, size_t count)
{
    Stdlib::AutoLock lock(Lock);

    MapSource src = { nullptr, nullptr, phyAddrs };

    return MapRangeLocked(virtAddr, count, src);
}

void PageTable::UnmapPages(ulong virtAddr, size_t count, bool freePages)
{
    Stdlib::AutoLock lock(Lock);

    UnmapRangeLocked(virtAddr, count, freePages);
}

ulong PageTable::MapMmioRegion(ulong physAddr, ulong sizeBytes, MmioCachePolicy policy)
{
    const bool writeCombining =
        (policy == MmioWriteCombining) && Hal::IsWriteCombiningAvailable();

    /* The arch's premapped window is device-typed, so it can only serve an
       uncached request; a write-combining one builds its own leaf PTEs. */
    if (policy == MmioUncached)
    {
        ulong premapped = Hal::MmioPremappedVa(physAddr, sizeBytes);
        if (premapped != 0)
            return premapped;
    }

    if (physAddr & (Const::PageSize - 1))
        return 0;

    ulong pages = (sizeBytes + Const::PageSize - 1) / Const::PageSize;
    if (pages == 0)
        pages = 1;

    for (ulong i = 0; i < pages; i++)
    {
        ulong pa = physAddr + i * Const::PageSize;
        ulong va = pa + MemoryMap::KernelSpaceBase;

        Stdlib::AutoLock lock(Lock);

        ulong l4Index = Pte::L4Index(va);
        ulong l3Index = Pte::L3Index(va);
        ulong l2Index = Pte::L2Index(va);
        ulong l1Index = Pte::L1Index(va);

        if (Root == 0)
            return 0;

        PtePage* l4Page = (PtePage*)TmpMapPage(Root);
        if (l4Page == nullptr)
            return 0;

        Pte *l4Entry = &l4Page->Entry[l4Index];
        if (!l4Entry->Present()) {
            Page* p = AllocPageNoLock();
            if (!p) { TmpUnmapPage((ulong)l4Page); return 0; }
            l4Entry->SetAddress(p->GetPhyAddress());
            l4Entry->SetWritable();
            l4Entry->SetPresent();
        }

        PtePage* l3Page = (PtePage*)TmpMapPage(l4Entry->Address());
        TmpUnmapPage((ulong)l4Page);
        if (l3Page == nullptr)
            return 0;

        Pte *l3Entry = &l3Page->Entry[l3Index];
        if (!l3Entry->Present()) {
            Page* p = AllocPageNoLock();
            if (!p) { TmpUnmapPage((ulong)l3Page); return 0; }
            l3Entry->SetAddress(p->GetPhyAddress());
            l3Entry->SetWritable();
            l3Entry->SetPresent();
        }

        PtePage* l2Page = (PtePage*)TmpMapPage(l3Entry->Address());
        TmpUnmapPage((ulong)l3Page);
        if (l2Page == nullptr)
            return 0;

        Pte *l2Entry = &l2Page->Entry[l2Index];
        if (!l2Entry->Present()) {
            Page* p = AllocPageNoLock();
            if (!p) { TmpUnmapPage((ulong)l2Page); return 0; }
            l2Entry->SetAddress(p->GetPhyAddress());
            l2Entry->SetWritable();
            l2Entry->SetPresent();
        }

        PtePage* l1Page = (PtePage*)TmpMapPage(l2Entry->Address());
        TmpUnmapPage((ulong)l2Page);
        if (l1Page == nullptr)
            return 0;

        Pte *l1Entry = &l1Page->Entry[l1Index];
        if (!l1Entry->Present())
        {
            l1Entry->SetAddress(pa);
            if (writeCombining)
                l1Entry->SetWriteCombining();
            else
                l1Entry->SetCacheDisabled();   /* device memory: uncached */
            /* Nothing is ever executed out of MMIO */
            l1Entry->SetNoExecute();
            l1Entry->SetWritable();
            l1Entry->SetPresent();
        }
        TmpUnmapPage((ulong)l1Page);
        Hal::TlbFlushPage(va);
    }

    return physAddr + MemoryMap::KernelSpaceBase;
}

/* Legacy single-page form: returns the page so the caller can pair its own
   FreePage/Put with it, the way it did before UnmapPages existed. */
Page* PageTable::UnmapPage(ulong virtAddr)
{
    Stdlib::AutoLock lock(Lock);

    PtePage* l1Page = WalkToL1Locked(virtAddr, false);
    if (l1Page == nullptr)
        return nullptr;

    BugOn(virtAddr & (Const::PageSize - 1));
    BugOn(virtAddr >= TmpMapStart && virtAddr < (TmpMapStart + Stdlib::ArraySize(TmpMapPageArray) * Const::PageSize));

    Pte* l1Entry = &l1Page->Entry[Pte::L1Index(virtAddr)];
    if (!l1Entry->Present())
    {
        BugOn(1);
        TmpUnmapPage((ulong)l1Page);
        return nullptr;
    }

    ulong phyAddr = l1Entry->Address();
    l1Entry->Clear();
    TmpUnmapPage((ulong)l1Page);
    BugOn(!phyAddr);
    Hal::TlbFlushPage(virtAddr);

    Page* page = GetPage(phyAddr);
    page->Put();
    return page;
}

ulong PageTable::GetFreePagesCount()
{
    Stdlib::AutoLock lock(Lock);
    return FreePagesCount;
}

ulong PageTable::GetTotalPagesCount()
{
    Stdlib::AutoLock lock(Lock);
    return TotalPagesCount;
}

ulong PageTable::GetVaEnd()
{
    /* Rounded to the huge page, not to the 4KiB one: PageArray is mapped in
       2MiB blocks, and the VA allocator starts here. Handing it the tail of
       a block that is already mapped would have it build 4KiB mappings under
       an L2 entry that is a leaf. */
    return Stdlib::RoundUp((ulong)&PageArray[PageArrayCount], HugePageSize);
}

}
}