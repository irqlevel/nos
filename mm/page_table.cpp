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
    , AvailableFreePagesCount(0)
    , FreePagesCount(0)
{
    for (size_t i = 0; i < Stdlib::ArraySize(TmpMapUsage); i++)
    {
        TmpMapUsage[i] = false;
    }
    TmpMapL1Page = nullptr;
}

PageTable::~PageTable()
{
}

bool PageTable::GetFreePages()
{
    auto& mmap = MemoryMap::GetInstance();
    ulong memStart, memEnd;

    if (!mmap.FindRegion(BuiltinPageTable::GetInstance().VirtToPhys(mmap.GetKernelEnd()), 4 * Const::GB, memStart, memEnd))
    {
        Trace(0, "Can't get available memory region");
        return false;
    }

    Trace(0, "Phy memStart 0x%p memEnd 0x%p", memStart, memEnd);

    if (memStart % Const::PageSize)
    {
        Trace(0, "Invalid phy memory start 0x%p", memStart);
        return false;
    }

    if (memEnd % Const::PageSize)
    {
        Trace(0, "Invalid phy memory end 0x%p", memEnd);
        return false;
    }

    for (ulong address = memStart; address < memEnd; address+= Const::PageSize)
    {
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
        FreePagesCount++;
    }

    AvailableFreePagesCount = FreePagesCount;
    Trace(0, "AvailableFreePagesCount %u", AvailableFreePagesCount);
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
    AvailableFreePagesCount--;
    Trace(0, "GetFreePage 0x%p", curr);
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

    return true;
}

bool PageTable::Setup()
{
    if (!GetFreePages())
        return false;

    auto& mmap = MemoryMap::GetInstance();
    for (ulong address = mmap.GetKernelStart(); address < mmap.GetKernelEnd(); address+= Const::PageSize)
    {
        if (!SetupPage(address, BuiltinPageTable::GetInstance().VirtToPhys(address)))
        {
            Trace(0, "can't setup page");
            return false;
        }
    }

    TmpMapL1Page = (PtePage *)mmap.GetKernelEnd();

    TmpMapStart = Stdlib::RoundUp(mmap.GetKernelEnd() + Const::PageSize, 512 * Const::PageSize);
    Trace(0, "TmpMapStart 0x%p", TmpMapStart);
    for (size_t i = 0; i < Stdlib::ArraySize(TmpMapUsage); i++)
    {
        if (!SetupPage(TmpMapStart + i * Const::PageSize, 0))
            return false;

        TmpMapUsage[i] = false;
    }

    auto tmpMapL1PagePhyAddr = GetL1Page(TmpMapStart);
    for (size_t i = 0; i < Stdlib::ArraySize(TmpMapUsage); i++)
        BugOn(tmpMapL1PagePhyAddr != GetL1Page(TmpMapStart + i * Const::PageSize));

    if (!SetupPage((ulong)TmpMapL1Page, tmpMapL1PagePhyAddr))
        return false;

    if (!SetupPage(0, 0))
        return false;

    return true;
}

ulong PageTable::MapPage(ulong phyAddr)
{
    Stdlib::AutoLock lock(TmpMapLock);

    BugOn(phyAddr & (Const::PageSize - 1));

    for (size_t i = 0; i < Stdlib::ArraySize(TmpMapUsage); i++)
    {
        if (!TmpMapUsage[i]) {
            ulong virtAddr = TmpMapStart + i * Const::PageSize;
            ulong l1Index = (virtAddr >> (12 + 0 * 9)) & ((1 << 9) - 1);
            Pte *l1Entry = &TmpMapL1Page->Entry[l1Index];
            if (l1Entry->Present())
                return false;

            l1Entry->SetAddress(phyAddr);
            l1Entry->SetCacheDisabled();
            l1Entry->SetWritable();
            l1Entry->SetPresent();
            Invlpg(virtAddr);
            TmpMapUsage[i] = true;
            //Trace(0, "Map 0x%p 0x%p -> 0x%p", l1Entry, phyAddr, virtAddr);
            return virtAddr;
        }
    }

    return 0;
}

ulong PageTable::UnmapPage(ulong virtAddr)
{
    Stdlib::AutoLock lock(TmpMapLock);

    BugOn(virtAddr & (Const::PageSize - 1));
    BugOn(!virtAddr);
    BugOn(virtAddr < TmpMapStart);
    BugOn(virtAddr >= (TmpMapStart + Stdlib::ArraySize(TmpMapUsage) * Const::PageSize));
    size_t i = (virtAddr - TmpMapStart) / Const::PageSize;
    BugOn(!TmpMapUsage[i]);

    ulong l1Index = (virtAddr >> (12 + 0 * 9)) & ((1 << 9) - 1);
    Pte *l1Entry = &TmpMapL1Page->Entry[l1Index];
    BugOn(!l1Entry->Present());
    auto phyAddr = l1Entry->Address();
    l1Entry->Clear();
    Invlpg(virtAddr);
    TmpMapUsage[i] = false;
    //Trace(0, "Unmap 0x%p 0x%p -> 0x%p", l1Entry, phyAddr, virtAddr);

    return phyAddr;
}

ulong PageTable::GetRoot()
{
    Stdlib::AutoLock lock(Lock);
    return Root;
}

ulong PageTable::AllocPage()
{
    Stdlib::AutoLock lock(FreePagesLock);

    if (FreePages == 0)
        return 0;

    ulong curr = FreePages;
    ulong currva = MapPage(curr);
    if (currva == 0)
        return 0;

    ulong next = *(ulong *)currva;
    FreePages = next;

    Stdlib::MemSet((void *)currva, 0, Const::PageSize);
    UnmapPage(currva);
    AvailableFreePagesCount--;
    return curr;
}

void PageTable::FreePage(ulong phyAddr)
{
    Stdlib::AutoLock lock(FreePagesLock);

    ulong currva = MapPage(phyAddr);
    *(ulong *)currva = FreePages;
    UnmapPage(currva);
    FreePages = phyAddr;
    AvailableFreePagesCount++;
}

ulong PageTable::MapAddress(ulong phyAddr)
{
    ulong phyPage = phyAddr & ~(Const::PageSize - 1);
    ulong vaPage = MapPage(phyPage);
    if (!vaPage)
        return 0;

    return vaPage + (phyAddr - phyPage);
}

bool PageTable::MapPage(ulong virtAddr, ulong phyAddr)
{
    Stdlib::AutoLock lock(Lock);

    BugOn(virtAddr & (Const::PageSize - 1));
    BugOn(phyAddr & (Const::PageSize - 1));
    BugOn(virtAddr >= TmpMapStart && virtAddr < (TmpMapStart + Stdlib::ArraySize(TmpMapUsage)));

    ulong l4Index = (virtAddr >> (12 + 3 * 9)) & ((1 << 9) - 1);
    ulong l3Index = (virtAddr >> (12 + 2 * 9)) & ((1 << 9) - 1);
    ulong l2Index = (virtAddr >> (12 + 1 * 9)) & ((1 << 9) - 1);
    ulong l1Index = (virtAddr >> (12 + 0 * 9)) & ((1 << 9) - 1);

    if (Root == 0)
    {
        return false;
    }

    PtePage* l4Page = (PtePage*)MapPage(Root);
    if (l4Page == nullptr)
        return false;

    Pte *l4Entry = &l4Page->Entry[l4Index];
    if (!l4Entry->Present()) {
        ulong addr = AllocPage();
        if (addr == 0) {
            UnmapPage((ulong)l4Page);
            return false;
        }

        l4Entry->SetAddress(addr);
        l4Entry->SetCacheDisabled();
        l4Entry->SetWritable();
        l4Entry->SetPresent();
    }

    PtePage* l3Page = (PtePage*)MapPage(l4Entry->Address());
    UnmapPage((ulong)l4Page);
    if (l3Page == nullptr)
        return false;

    Pte *l3Entry = &l3Page->Entry[l3Index];
    if (!l3Entry->Present()) {
        ulong addr = AllocPage();
        if (addr == 0) {
            UnmapPage((ulong)l3Page);
            return false;
        }
        l3Entry->SetAddress(addr);
        l3Entry->SetCacheDisabled();
        l3Entry->SetWritable();
        l3Entry->SetPresent();
    }

    PtePage* l2Page = (PtePage*)MapPage(l3Entry->Address());
    UnmapPage((ulong)l3Page);
    if (l2Page == nullptr)
        return false;

    Pte *l2Entry = &l2Page->Entry[l2Index];
    if (!l2Entry->Present()) {
        ulong addr = AllocPage();
        if (addr == 0) {
            UnmapPage((ulong)l2Page);
            return false;
        }

        l2Entry->SetAddress(addr);
        l2Entry->SetCacheDisabled();
        l2Entry->SetWritable();
        l2Entry->SetPresent();
    }

    PtePage* l1Page = (PtePage*)MapPage(l2Entry->Address());
    UnmapPage((ulong)l2Page);
    if (l1Page == nullptr)
        return false;

    Pte *l1Entry = &l1Page->Entry[l1Index];
    if (l1Entry->Present()) {
        UnmapPage((ulong)l1Page);
        return false;
    }

    if (phyAddr)
    {
        l1Entry->SetAddress(phyAddr);
        l1Entry->SetCacheDisabled();
        l1Entry->SetWritable();
        l1Entry->SetPresent();
    } else {
        l1Entry->Clear();
    }
    UnmapPage((ulong)l1Page);

    return true;
}

ulong PageTable::GetAvailableFreePages()
{
    Stdlib::AutoLock lock(Lock);
    return AvailableFreePagesCount;
}

ulong PageTable::GetTmpMapEnd()
{
    return TmpMapStart + Stdlib::ArraySize(TmpMapUsage) * Const::PageSize;
}

}
}