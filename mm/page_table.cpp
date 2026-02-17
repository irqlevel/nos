#include "page_table.h"
#include "memory_map.h"

#include <kernel/trace.h>
#include <kernel/asm.h>
#include <kernel/debug.h>

namespace Kernel
{

namespace Mm
{

BuiltinPageTable::BuiltinPageTable()
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

bool BuiltinPageTable::Setup()
{
    //Map first 4GB of kernel address space
    auto& p4Entry = P4Page.Entry[256];

    p4Entry.SetAddress(VirtToPhys((ulong)&P3KernelPage));
    p4Entry.SetCacheDisabled();
    p4Entry.SetWritable();
    p4Entry.SetPresent();

    ulong addr = MemoryMap::KernelSpaceBase;
    for (size_t i = 0; i < 4; i++)
    {
        auto& p3Entry = P3KernelPage.Entry[i];
        auto& p2Page = P2KernelPage[i];

        p3Entry.SetAddress(VirtToPhys((ulong)&p2Page));
        p3Entry.SetCacheDisabled();
        p3Entry.SetWritable();
        p3Entry.SetPresent();

        for (size_t j = 0; j < 512; j++)
        {
            auto& p2Entry = p2Page.Entry[j];

            p2Entry.SetAddress(VirtToPhys(addr));
            p2Entry.SetCacheDisabled();
            p2Entry.SetWritable();
            p2Entry.SetHuge();
            p2Entry.SetPresent();
            Invlpg(PhysToVirt(addr));

            addr += (2 * Const::MB);
        }
    }

    //Map first 4GB of user address space

    auto& p4Entry2 = P4Page.Entry[0];

    p4Entry2.SetAddress(VirtToPhys((ulong)&P3UserPage));
    p4Entry2.SetCacheDisabled();
    p4Entry2.SetWritable();
    p4Entry2.SetPresent();

    addr = 0;
    for (size_t i = 0; i < 4; i++)
    {
        auto& p3Entry = P3UserPage.Entry[i];
        auto& p2Page = P2UserPage[i];

        p3Entry.SetAddress(VirtToPhys((ulong)&p2Page));
        p3Entry.SetCacheDisabled();
        p3Entry.SetWritable();
        p3Entry.SetPresent();
        Invlpg((ulong)&p2Page);

        for (size_t j = 0; j < 512; j++)
        {
            auto& p2Entry = p2Page.Entry[j];

            p2Entry.SetAddress(VirtToPhys(addr));
            p2Entry.SetCacheDisabled();
            p2Entry.SetWritable();
            p2Entry.SetHuge();
            p2Entry.SetPresent();
            Invlpg(PhysToVirt(addr));

            addr += (2 * Const::MB);
        }
    }

    P2UserPage[0].Entry[0].Value = 0;
    Invlpg(0);

    return true;
}

ulong BuiltinPageTable::GetRoot()
{
    return VirtToPhys((ulong)&P4Page);
}

BuiltinPageTable::~BuiltinPageTable()
{
}

PageTable::PageTable()
    : Root(0)
    , PageArray(nullptr)
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

bool PageTable::GetFreePages()
{
    auto& mmap = MemoryMap::GetInstance();
    const ulong BuiltinMapLimit = 4UL * Const::GB;

    for (size_t i = 0; i < mmap.GetRegionCount(); i++)
    {
        ulong addr, len, type;

        if (!mmap.GetRegion(i, addr, len, type))
            return false;

        if (type != 1)
            continue;

        ulong limit = Stdlib::RoundUp(addr + len, Const::PageSize);
        if (limit > BuiltinMapLimit)
            limit = BuiltinMapLimit;
        if (limit > HighestPhyAddr)
            HighestPhyAddr = limit;

        ulong memStart = Stdlib::RoundUp(addr, Const::PageSize);
        ulong memEnd = ((addr + len) / Const::PageSize) * Const::PageSize;

        if (memStart < Const::MB)
            memStart = Stdlib::RoundUp(Const::MB, Const::PageSize);

        if (memEnd > BuiltinMapLimit)
            memEnd = BuiltinMapLimit;

        Trace(0, "Phy memStart 0x%p memEnd 0x%p", memStart, memEnd);

        if (memStart >= memEnd)
            continue;

        Trace(0, "GetFreePages: entering inner loop memStart 0x%p memEnd 0x%p", memStart, memEnd);
        for (ulong address = memStart; address < memEnd; address+= Const::PageSize)
        {
            if (address == memStart || (TotalPagesCount % 100000) == 0)
                Trace(0, "GetFreePages: addr 0x%p total %u", address, TotalPagesCount);
            TotalPagesCount++;

            if (BuiltinPageTable::GetInstance().PhysToVirt(address) >= mmap.GetKernelStart()
                && BuiltinPageTable::GetInstance().PhysToVirt(address) < mmap.GetKernelEnd())
                continue;

            if (FreePages == 0)
            {
                *(ulong *)BuiltinPageTable::GetInstance().PhysToVirt(address) = 0;
                FreePages = address;
            }
            else
            {
                ulong next = FreePages;
                FreePages = address;
                *(ulong *)BuiltinPageTable::GetInstance().PhysToVirt(address) = next;
            }
        }
        Trace(0, "GetFreePages: inner loop done, total %u", TotalPagesCount);
    }

    Trace(0, "GetFreePages done, HighestPhyAddr 0x%p TotalPages %u", HighestPhyAddr, TotalPagesCount);

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

ulong PageTable::GetFreePageByTmpMap()
{
    if (FreePages == 0)
        return 0;

    ulong curr = FreePages;
    ulong va = TmpMapPage(curr);
    ulong next = *(ulong *)va;
    FreePages = next;

    Stdlib::MemSet((void*)va, 0, Const::PageSize);
    TmpUnmapPage(va);

    return curr;
}

ulong PageTable::GetL1Page(ulong virtAddr)
{
    BugOn(virtAddr & (Const::PageSize - 1));
    BugOn(!virtAddr);

    ulong l4Index = (virtAddr >> (12 + 3 * 9)) & ((1 << 9) - 1);
    ulong l3Index = (virtAddr >> (12 + 2 * 9)) & ((1 << 9) - 1);
    ulong l2Index = (virtAddr >> (12 + 1 * 9)) & ((1 << 9) - 1);

    if (!Root)
        return 0;

    PtePage* l4Page = (PtePage*)BuiltinPageTable::GetInstance().PhysToVirt(Root);
    Pte *l4Entry = &l4Page->Entry[l4Index];
    if (!l4Entry->Present())
        return 0;

    PtePage* l3Page = (PtePage*)BuiltinPageTable::GetInstance().PhysToVirt(l4Entry->Address());
    Pte *l3Entry = &l3Page->Entry[l3Index];
    if (!l3Entry->Present())
        return 0;

    PtePage* l2Page = (PtePage*)BuiltinPageTable::GetInstance().PhysToVirt(l3Entry->Address());
    Pte *l2Entry = &l2Page->Entry[l2Index];
    if (!l2Entry->Present())
        return 0;

    return l2Entry->Address();
}

ulong PageTable::VirtToPhys(ulong virtAddr)
{
    if (!Root)
        return 0;

    ulong l4Index = (virtAddr >> (12 + 3 * 9)) & ((1 << 9) - 1);
    ulong l3Index = (virtAddr >> (12 + 2 * 9)) & ((1 << 9) - 1);
    ulong l2Index = (virtAddr >> (12 + 1 * 9)) & ((1 << 9) - 1);
    ulong l1Index = (virtAddr >> (12 + 0 * 9)) & ((1 << 9) - 1);
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

    PtePage* l2Page = (PtePage*)TmpMapPage(l3Entry.Address());
    if (!l2Page)
        return 0;
    Pte l2Entry = l2Page->Entry[l2Index];
    TmpUnmapPage((ulong)l2Page);
    if (!l2Entry.Present())
        return 0;

    /* 2MB huge page at L2 level */
    if (l2Entry.Value & (1UL << Pte::HugeBit))
    {
        ulong hugeOffset = virtAddr & ((1UL << 21) - 1);
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

    ulong l4Index = (virtAddr >> (12 + 3 * 9)) & ((1 << 9) - 1);
    ulong l3Index = (virtAddr >> (12 + 2 * 9)) & ((1 << 9) - 1);
    ulong l2Index = (virtAddr >> (12 + 1 * 9)) & ((1 << 9) - 1);
    ulong l1Index = (virtAddr >> (12 + 0 * 9)) & ((1 << 9) - 1);

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
        l4Entry->SetCacheDisabled();
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
        l3Entry->SetCacheDisabled();
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
        l2Entry->SetCacheDisabled();
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
        l1Entry->SetCacheDisabled();
        l1Entry->SetWritable();
        l1Entry->SetPresent();
    } else {
        l1Entry->Clear();
    }
    Invlpg(virtAddr);
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

void PageTable::ExcludeFreePages(ulong phyLimit)
{
    ulong curr = FreePages;
    ulong prev = 0;

    while (curr)
    {
        ulong next = *(ulong *)BuiltinPageTable::GetInstance().PhysToVirt(curr);
        if (curr < phyLimit)
        {
            if (prev)
                *(ulong *)BuiltinPageTable::GetInstance().PhysToVirt(prev) = next;
            else
                FreePages = next;
        } else {
            prev = curr;
        }
        curr = next;
    }
}

bool PageTable::Setup()
{
    Trace(0, "PageTable setup");

    if (!GetFreePages())
        return false;

    auto& mmap = MemoryMap::GetInstance();
    TmpMapL1Page = (PtePage *)mmap.GetKernelEnd();
    TmpMapStart = Stdlib::RoundUp(mmap.GetKernelEnd() + Const::PageSize, 512 * Const::PageSize);
    PageArray = (Page*)(TmpMapStart + Stdlib::ArraySize(TmpMapPageArray) * Const::PageSize);
    ulong pageArrayLimit = (ulong)PageArray + Stdlib::RoundUp((HighestPhyAddr/Const::PageSize + 1) * sizeof(Page), Const::PageSize);

    ExcludeFreePages(BuiltinPageTable::GetInstance().VirtToPhys(pageArrayLimit));

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

    ulong virtAddr = (ulong)PageArray;
    ulong phaSpace = 0, pha;
    for (ulong phyAddr = 0; phyAddr <= HighestPhyAddr; phyAddr += Const::PageSize)
    {
        if (phaSpace == 0)
        {
            pha = GetFreePage();
            if (!pha)
                return false;

            if (!SetupPage(virtAddr, pha))
                return false;
            phaSpace = Const::PageSize;
        }
        Page *page = (Page *)BuiltinPageTable::GetInstance().PhysToVirt(pha + Const::PageSize - phaSpace);
        page->Init(phyAddr);
        phaSpace -= sizeof(Page);
        virtAddr += sizeof(Page);
        PageArrayCount++;
    }

    Trace(0, "PageArray setup done, count %u", PageArrayCount);
    return true;
}

bool PageTable::SetupFreePagesList()
{
    FreePagesCount = 0;
    for (;;)
    {
        ulong phyAddr = GetFreePageByTmpMap();
        if (!phyAddr)
            break;

        Page* page = GetPage(phyAddr);
        BugOn(!page);
        FreePagesList.InsertHead(&page->ListEntry);
        FreePagesCount++;
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

void PageTable::FreePage(Page* page)
{
    Stdlib::AutoLock lock(Lock);

    FreePagesCount++;
    FreePagesList.InsertHead(&page->ListEntry);
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
            ulong l1Index = (virtAddr >> (12 + 0 * 9)) & ((1 << 9) - 1);
            Pte *l1Entry = &TmpMapL1Page->Entry[l1Index];
            if (l1Entry->Present())
                return 0;

            l1Entry->SetAddress(phyAddr);
            l1Entry->SetCacheDisabled();
            l1Entry->SetWritable();
            l1Entry->SetPresent();
            Invlpg(virtAddr);

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

    ulong l1Index = (virtAddr >> (12 + 0 * 9)) & ((1 << 9) - 1);
    Pte *l1Entry = &TmpMapL1Page->Entry[l1Index];
    BugOn(!l1Entry->Present());
    auto phyAddr = l1Entry->Address();
    l1Entry->Clear();
    Invlpg(virtAddr);
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
    SetCr3(GetCr3());
}

void PageTable::InvalidateLocalTlbAddress(ulong virtAddr)
{
    Invlpg(virtAddr);
}

void PageTable::InvalidateLocalTlbRange(ulong virtAddr, ulong count)
{
    for (ulong i = 0; i < count; i++)
        Invlpg(virtAddr + i * Const::PageSize);
}

ulong PageTable::TmpMapAddress(ulong phyAddr)
{
    ulong phyPage = phyAddr & ~(Const::PageSize - 1);
    ulong vaPage = TmpMapPage(phyPage);
    if (!vaPage)
        return 0;

    return vaPage + (phyAddr - phyPage);
}

bool PageTable::MapPage(ulong virtAddr, Page* page)
{
    Stdlib::AutoLock lock(Lock);

    BugOn(virtAddr & (Const::PageSize - 1));
    BugOn(virtAddr >= TmpMapStart && virtAddr < (TmpMapStart + Stdlib::ArraySize(TmpMapPageArray) * Const::PageSize));

    ulong l4Index = (virtAddr >> (12 + 3 * 9)) & ((1 << 9) - 1);
    ulong l3Index = (virtAddr >> (12 + 2 * 9)) & ((1 << 9) - 1);
    ulong l2Index = (virtAddr >> (12 + 1 * 9)) & ((1 << 9) - 1);
    ulong l1Index = (virtAddr >> (12 + 0 * 9)) & ((1 << 9) - 1);

    if (Root == 0)
    {
        return false;
    }

    PtePage* l4Page = (PtePage*)TmpMapPage(Root);
    if (l4Page == nullptr)
        return false;

    Pte *l4Entry = &l4Page->Entry[l4Index];
    if (!l4Entry->Present()) {
        Page* page = AllocPageNoLock();
        if (!page) {
            TmpUnmapPage((ulong)l4Page);
            return false;
        }

        l4Entry->SetAddress(page->GetPhyAddress());
        l4Entry->SetCacheDisabled();
        l4Entry->SetWritable();
        l4Entry->SetPresent();
    }

    PtePage* l3Page = (PtePage*)TmpMapPage(l4Entry->Address());
    TmpUnmapPage((ulong)l4Page);
    if (l3Page == nullptr)
        return false;

    Pte *l3Entry = &l3Page->Entry[l3Index];
    if (!l3Entry->Present()) {
        Page *page = AllocPageNoLock();
        if (!page) {
            TmpUnmapPage((ulong)l3Page);
            return false;
        }
        l3Entry->SetAddress(page->GetPhyAddress());
        l3Entry->SetCacheDisabled();
        l3Entry->SetWritable();
        l3Entry->SetPresent();
    }

    PtePage* l2Page = (PtePage*)TmpMapPage(l3Entry->Address());
    TmpUnmapPage((ulong)l3Page);
    if (l2Page == nullptr)
        return false;

    Pte *l2Entry = &l2Page->Entry[l2Index];
    if (!l2Entry->Present()) {
        Page* page = AllocPageNoLock();
        if (!page) {
            TmpUnmapPage((ulong)l2Page);
            return false;
        }

        l2Entry->SetAddress(page->GetPhyAddress());
        l2Entry->SetCacheDisabled();
        l2Entry->SetWritable();
        l2Entry->SetPresent();
    }

    PtePage* l1Page = (PtePage*)TmpMapPage(l2Entry->Address());
    TmpUnmapPage((ulong)l2Page);
    if (l1Page == nullptr)
        return false;

    Pte *l1Entry = &l1Page->Entry[l1Index];
    if (l1Entry->Present()) {
        TmpUnmapPage((ulong)l1Page);
        return false;
    }

    if (page)
    {
        page->Get();
        l1Entry->SetAddress(page->GetPhyAddress());
        l1Entry->SetCacheDisabled();
        l1Entry->SetWritable();
        l1Entry->SetPresent();
    } else {
        l1Entry->Clear();
    }
    TmpUnmapPage((ulong)l1Page);
    Invlpg(virtAddr);

    return true;
}

ulong PageTable::MapMmioRegion(ulong physAddr, ulong sizeBytes)
{
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

        ulong l4Index = (va >> (12 + 3 * 9)) & ((1 << 9) - 1);
        ulong l3Index = (va >> (12 + 2 * 9)) & ((1 << 9) - 1);
        ulong l2Index = (va >> (12 + 1 * 9)) & ((1 << 9) - 1);
        ulong l1Index = (va >> (12 + 0 * 9)) & ((1 << 9) - 1);

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
            l4Entry->SetCacheDisabled();
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
            l3Entry->SetCacheDisabled();
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
            l2Entry->SetCacheDisabled();
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
            l1Entry->SetCacheDisabled();
            l1Entry->SetWritable();
            l1Entry->SetPresent();
        }
        TmpUnmapPage((ulong)l1Page);
        Invlpg(va);
    }

    return physAddr + MemoryMap::KernelSpaceBase;
}

Page* PageTable::UnmapPage(ulong virtAddr)
{
    Stdlib::AutoLock lock(Lock);

    BugOn(virtAddr & (Const::PageSize - 1));
    BugOn(virtAddr >= TmpMapStart && virtAddr < (TmpMapStart + Stdlib::ArraySize(TmpMapPageArray) * Const::PageSize));

    ulong l4Index = (virtAddr >> (12 + 3 * 9)) & ((1 << 9) - 1);
    ulong l3Index = (virtAddr >> (12 + 2 * 9)) & ((1 << 9) - 1);
    ulong l2Index = (virtAddr >> (12 + 1 * 9)) & ((1 << 9) - 1);
    ulong l1Index = (virtAddr >> (12 + 0 * 9)) & ((1 << 9) - 1);

    if (Root == 0)
    {
        BugOn(1);
        return nullptr;
    }

    PtePage* l4Page = (PtePage*)TmpMapPage(Root);
    if (l4Page == nullptr)
        return nullptr;

    Pte *l4Entry = &l4Page->Entry[l4Index];
    if (!l4Entry->Present())
    {
        BugOn(1);
        TmpUnmapPage((ulong)l4Page);
        return nullptr;
    }

    PtePage* l3Page = (PtePage*)TmpMapPage(l4Entry->Address());
    TmpUnmapPage((ulong)l4Page);
    if (l3Page == nullptr)
        return nullptr;

    Pte *l3Entry = &l3Page->Entry[l3Index];
    if (!l3Entry->Present()) {
        BugOn(1);
        TmpUnmapPage((ulong)l3Page);
        return nullptr;
    }

    PtePage* l2Page = (PtePage*)TmpMapPage(l3Entry->Address());
    TmpUnmapPage((ulong)l3Page);
    if (l2Page == nullptr)
        return nullptr;

    Pte *l2Entry = &l2Page->Entry[l2Index];
    if (!l2Entry->Present()) {
        BugOn(1);
        TmpUnmapPage((ulong)l2Page);
        return nullptr;
    }

    PtePage* l1Page = (PtePage*)TmpMapPage(l2Entry->Address());
    TmpUnmapPage((ulong)l2Page);
    if (l1Page == nullptr)
        return nullptr;

    Pte *l1Entry = &l1Page->Entry[l1Index];
    if (!l1Entry->Present()) {
        BugOn(1);
        TmpUnmapPage((ulong)l1Page);
        return nullptr;
    }

    ulong phyAddr = l1Entry->Address();
    l1Entry->Clear();
    TmpUnmapPage((ulong)l1Page);
    BugOn(!phyAddr);
    Invlpg(virtAddr);
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
    return Stdlib::RoundUp((ulong)&PageArray[PageArrayCount], Const::PageSize);
}

}
}