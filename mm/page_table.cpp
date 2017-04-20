#include "page_table.h"
#include "memory_map.h"

#include <kernel/trace.h>

namespace Kernel
{


// N pages
// N * sizeof(page)
// N * 8 = sizeof page entries

PageTable::PageTable()
{
    Shared::MemSet(&P4Page, 0, sizeof(P4Page));

    Shared::MemSet(&P3KernelPage, 0, sizeof(P3KernelPage));
    Shared::MemSet(&P3UserPage, 0, sizeof(P3UserPage));

    Shared::MemSet(&P2KernelPage[0], 0, sizeof(P2KernelPage));
    Shared::MemSet(&P2UserPage[0], 0, sizeof(P2UserPage));

    Trace(0, "PageTable 0x%p P4Page 0x%p", this, &P4Page);
}

ulong PageTable::VirtToPhys(ulong virtAddr)
{
    BugOn(virtAddr > MemoryMap::UserSpaceMax && virtAddr < MemoryMap::KernelSpaceBase);

    if (virtAddr <= MemoryMap::UserSpaceMax)
        return virtAddr;

    return virtAddr - MemoryMap::KernelSpaceBase;
}

ulong PageTable::PhysToVirt(ulong phyAddr)
{
    return phyAddr + MemoryMap::KernelSpaceBase;
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

            addr += (2 * Shared::MB);
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

            addr += (2 * Shared::MB);
        }
    }

    return true;
}

ulong PageTable::GetRoot()
{
    return VirtToPhys((ulong)&P4Page);
}

PageTable::~PageTable()
{
}

}