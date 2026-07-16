#include <mm/page_table.h>
#include <mm/memory_map.h>

#include <kernel/trace.h>
#include <hal/mmu.h>

#include "board.h"

namespace Kernel
{

namespace Mm
{

/* arm64 bootstrap linear map, the C++ twin of the tables boot.S builds:
   TTBR1 L0[256] -> L1[0..3] -> 512x2MiB L2 blocks covering the first 4GiB
   at KernelSpaceBase. Same 9-9-9-9-12 geometry as x86, so the same class
   layout (P4Page = L0, P3KernelPage = L1, P2KernelPage = L2) is reused.
   Differences from x86: physical ranges below the RAM base are mapped
   Device-nGnRE (no MTRRs to fix cacheability), and there is no low-half
   identity map — user VAs go through TTBR0, which stays empty. */

/* All QEMU-virt MMIO sits below the RAM base and is covered by the 1GiB
   Device-nGnRE L1 block InstallEarlyDeviceBlock puts into the real table. */
ulong EarlyDeviceLimit;

bool BuiltinPageTable::Setup()
{
    auto& board = Board::GetInstance();
    ulong ramBase = board.MemRegions[0].Addr;

    auto& l0Entry = P4Page.Entry[256];

    l0Entry.SetAddress(VirtToPhys((ulong)&P3KernelPage));
    l0Entry.SetWritable();
    l0Entry.SetPresent();

    ulong addr = MemoryMap::KernelSpaceBase;
    for (size_t i = 0; i < 4; i++)
    {
        auto& l1Entry = P3KernelPage.Entry[i];
        auto& l2Page = P2KernelPage[i];

        l1Entry.SetAddress(VirtToPhys((ulong)&l2Page));
        l1Entry.SetWritable();
        l1Entry.SetPresent();

        for (size_t j = 0; j < 512; j++)
        {
            auto& l2Entry = l2Page.Entry[j];

            l2Entry.SetAddress(VirtToPhys(addr));
            l2Entry.SetWritable();
            l2Entry.SetHuge();
            l2Entry.SetPresent();
            if (VirtToPhys(addr) < ramBase)
                l2Entry.SetCacheDisabled();

            addr += (2 * Const::MB);
        }
    }

    return true;
}

/* Install one 1GiB Device-nGnRE L1 block covering phys [0, ramBase-ish)
   into the freshly built real page table, so PL011/GIC/virtio survive the
   builtin->real root switch (x86 has no such need: its console is port
   I/O and MMIO regions are mapped later via MapMmioRegion). Must run
   BEFORE the switch: the real table's pages are edited through the still-
   active builtin linear map. */
bool InstallEarlyDeviceBlock(ulong realRoot)
{
    auto& bpt = BuiltinPageTable::GetInstance();

    PtePage* l0 = (PtePage*)bpt.PhysToVirt(realRoot);
    Pte* l0Entry = &l0->Entry[Pte::L4Index(MemoryMap::KernelSpaceBase)];
    if (!l0Entry->Present())
        return false;

    PtePage* l1 = (PtePage*)bpt.PhysToVirt(l0Entry->Address());
    Pte* l1Entry = &l1->Entry[0]; /* [KernelSpaceBase, +1GiB): all MMIO */
    if (l1Entry->Present())
        return false;

    l1Entry->SetAddress(0);
    l1Entry->SetWritable();
    l1Entry->SetHuge();
    l1Entry->SetCacheDisabled();
    l1Entry->SetPresent();

    EarlyDeviceLimit = 1 * Const::GB;
    return true;
}

}
}

namespace Hal
{

ulong MmioPremappedVa(ulong physAddr, ulong sizeBytes)
{
    using Kernel::Mm::MemoryMap;
    if (Kernel::Mm::EarlyDeviceLimit != 0 &&
        physAddr + sizeBytes <= Kernel::Mm::EarlyDeviceLimit)
        return MemoryMap::KernelSpaceBase + physAddr;
    return 0;
}

}
