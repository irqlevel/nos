#include "page_table.h"
#include "memory_map.h"

#include <kernel/trace.h>
#include <kernel/asm.h>

namespace Kernel
{

namespace Mm
{

// N pages
// N * sizeof(page)
// N * 8 = sizeof page entries

PageTable::PageTable()
    : Pages(nullptr)
    , PageCount(0)
    , State(1)
{
    Stdlib::MemSet(&P4Page, 0, sizeof(P4Page));

    Stdlib::MemSet(&P3KernelPage, 0, sizeof(P3KernelPage));
    Stdlib::MemSet(&P3UserPage, 0, sizeof(P3UserPage));

    Stdlib::MemSet(&P2KernelPage[0], 0, sizeof(P2KernelPage));
    Stdlib::MemSet(&P2UserPage[0], 0, sizeof(P2UserPage));

    Trace(0, "PageTable 0x%p P4Page 0x%p", this, &P4Page);
}

ulong PageTable::VirtToPhys(ulong virtAddr)
{
    switch (State)
    {
    case 1:
    case 2:
        BugOn(virtAddr > MemoryMap::UserSpaceMax && virtAddr < MemoryMap::KernelSpaceBase);

        if (virtAddr <= MemoryMap::UserSpaceMax)
            return virtAddr;

        return virtAddr - MemoryMap::KernelSpaceBase;
    default:
        Panic("Invalid state");
        return -1;
    }
}

ulong PageTable::PhysToVirt(ulong phyAddr)
{
    switch (State)
    {
    case 1:
    case 2:
        return phyAddr + MemoryMap::KernelSpaceBase;
    default:
        Panic("Invalid state");
        return -1;
    }
}

bool PageTable::Setup()
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

        for (size_t j = 0; j < 512; j++)
        {
            auto& p2Entry = p2Page.Entry[j];

            p2Entry.SetAddress(VirtToPhys(addr));
            p2Entry.SetCacheDisabled();
            p2Entry.SetWritable();
            p2Entry.SetHuge();
            p2Entry.SetPresent();

            addr += (2 * Const::MB);
        }
    }

    State = 2;
    return true;
}

void PageTable::UnmapNull()
{
    switch (State)
    {
    case 2:
        P2UserPage[0].Entry[0].Value = 0;
        Invlpg(0);
        return;
    default:
        Panic("Invalid state");
    }
}

ulong PageTable::GetRoot()
{
    switch (State)
    {
    case 2:
        return VirtToPhys((ulong)&P4Page);
    default:
        Panic("Invalid state");
        return -1;
    }
}

PageTable::~PageTable()
{
}

bool PageTable::Setup2()
{
    auto& mmap = MemoryMap::GetInstance();
    ulong memStart, memEnd;

    if (!mmap.FindRegion(VirtToPhys(mmap.GetKernelEnd()), 4 * Const::GB, memStart, memEnd))
    {
        Trace(0, "Can't get available memory region");
        return false;
    }

    if (memStart % Const::PageSize)
    {
        Trace(0, "Invalid memory start");
        return false;
    }

    if (memEnd % Const::PageSize)
    {
        Trace(0, "Invalid memory end");
        return false;
    }

    size_t pageCount = memEnd / Const::PageSize;
    Trace(0, "Phy page count %u", pageCount);

    Page* pageStart = (Page*)PhysToVirt(memStart);
    for (size_t i = 0; i < pageCount; i++)
    {
        Page* page = &pageStart[i];
        if ((ulong)Stdlib::MemAdd(page, sizeof(*page)) >= PhysToVirt(memEnd))
        {
            Trace(0, "Pages array overflow memory end");
            return false;
        }

        page->Pfn = i;
        FreePagesList.InsertTail(&page->ListEntry);
    }

    return true;
}

}
}