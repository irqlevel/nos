#include <mm/page_table.h>
#include <mm/memory_map.h>

#include <kernel/trace.h>
#include <hal/mmu.h>

namespace Kernel
{

namespace Mm
{

/* The x86 bootstrap linear map: defined in an arch TU so other
   architectures supply their own BuiltinPageTable::Setup (see
   arch/arm64/builtin_pt.cpp). */

bool BuiltinPageTable::Setup()
{
    /* The whole 4GB range is mapped write-back: MTRRs keep MMIO holes
       (LAPIC, IOAPIC, PCI) uncached on real hardware. Setting PCD here
       would run the entire kernel uncached on bare metal. */

    //Map first 4GB of kernel address space
    auto& p4Entry = P4Page.Entry[256];

    p4Entry.SetAddress(VirtToPhys((ulong)&P3KernelPage));
    p4Entry.SetWritable();
    p4Entry.SetPresent();

    ulong addr = MemoryMap::KernelSpaceBase;
    for (size_t i = 0; i < 4; i++)
    {
        auto& p3Entry = P3KernelPage.Entry[i];
        auto& p2Page = P2KernelPage[i];

        p3Entry.SetAddress(VirtToPhys((ulong)&p2Page));
        p3Entry.SetWritable();
        p3Entry.SetPresent();

        for (size_t j = 0; j < 512; j++)
        {
            auto& p2Entry = p2Page.Entry[j];

            p2Entry.SetAddress(VirtToPhys(addr));
            p2Entry.SetWritable();
            p2Entry.SetHuge();
            p2Entry.SetPresent();
            Hal::TlbFlushPage(PhysToVirt(addr));

            addr += (2 * Const::MB);
        }
    }

    //Map first 4GB of user address space

    auto& p4Entry2 = P4Page.Entry[0];

    p4Entry2.SetAddress(VirtToPhys((ulong)&P3UserPage));
    p4Entry2.SetWritable();
    p4Entry2.SetPresent();

    addr = 0;
    for (size_t i = 0; i < 4; i++)
    {
        auto& p3Entry = P3UserPage.Entry[i];
        auto& p2Page = P2UserPage[i];

        p3Entry.SetAddress(VirtToPhys((ulong)&p2Page));
        p3Entry.SetWritable();
        p3Entry.SetPresent();
        Hal::TlbFlushPage((ulong)&p2Page);

        for (size_t j = 0; j < 512; j++)
        {
            auto& p2Entry = p2Page.Entry[j];

            p2Entry.SetAddress(VirtToPhys(addr));
            p2Entry.SetWritable();
            p2Entry.SetHuge();
            p2Entry.SetPresent();
            Hal::TlbFlushPage(PhysToVirt(addr));

            addr += (2 * Const::MB);
        }
    }

    P2UserPage[0].Entry[0].Value = 0;
    Hal::TlbFlushPage(0);

    return true;
}

}
}
