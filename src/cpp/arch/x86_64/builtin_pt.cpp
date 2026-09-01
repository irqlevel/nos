#include <mm/page_table.h>
#include <mm/memory_map.h>

#include <kernel/trace.h>
#include <hal/mmu.h>

#include "cpuid.h"

namespace Kernel
{

namespace Mm
{

/* The x86 bootstrap linear map: defined in an arch TU so other
   architectures supply their own BuiltinPageTable::Setup (see
   arch/arm64/builtin_pt.cpp). */

namespace
{

/* CPUID.80000001H:EDX[26] -- PDPE1GB: a PDPT entry may carry the page-size
   bit and map a 1GiB page on its own. Every x86-64 part since Barcelona and
   Nehalem has it; QEMU's default model does not advertise it, so the
   fallback below is a path that gets exercised rather than a theoretical
   one. */
bool CpuHasGiantPages()
{
    static const u32 ExtendedLeafBase = 0x80000000;
    static const u32 ExtendedFeatureLeaf = 0x80000001;
    static const u32 Pdpe1GbBit = 26;

    if (Cpuid(ExtendedLeafBase).Eax < ExtendedFeatureLeaf)
        return false;

    return (Cpuid(ExtendedFeatureLeaf).Edx & (1U << Pdpe1GbBit)) != 0;
}

}

bool BuiltinPageTable::Setup()
{
    /* The whole 4GB range is mapped write-back: MTRRs keep MMIO holes
       (LAPIC, IOAPIC, PCI) uncached on real hardware. Setting PCD here
       would run the entire kernel uncached on bare metal. */

    /* Four PDPT entries, each a page directory of 512 2MiB pages: 4GiB.
       That is the whole extent of the bootstrap map, and therefore the
       ceiling on the RAM the page allocator can take (see GetMappedLimit). */
    static const ulong KernelMapSize = 4 * Const::GB;

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

    MappedLimit = KernelMapSize;

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

void BuiltinPageTable::MapHighRam()
{
    /* The low 4GiB stays exactly as Setup() built it, at 2MiB granularity.
       Below that line the MTRRs describe MMIO holes with memory types other
       than write-back, and a large page spanning two MTRR types is left to
       the implementation (SDM 11.11.9) -- in practice the most conservative
       type wins, which would make a whole GiB of RAM uncached. Above 4GiB
       the default MTRR type covers RAM, and only GiB blocks that carry RAM
       are mapped at all, so the mixed case needs a firmware that puts MMIO
       and RAM inside one GiB up there. Even then MTRRs still force the MMIO
       part uncached; the cost would be speed, not correctness. */
    static const ulong LowMapLimit = 4 * Const::GB;
    static const ulong BlockSize = Const::GB;

    /* One PDPT entry per GiB, 512 of them under the kernel's P4 slot. */
    const ulong TopLimit = Stdlib::ArraySize(P3KernelPage.Entry) * BlockSize;

    BugOn(MappedLimit != LowMapLimit);

    auto& mmap = MemoryMap::GetInstance();
    ulong ramEnd = mmap.GetUsableRamEnd();
    if (ramEnd <= LowMapLimit)
        return;

    if (!CpuHasGiantPages())
    {
        Trace(0, "mm: no 1GiB page support, RAM above 0x%p stays unusable",
            LowMapLimit);
        return;
    }

    if (ramEnd > TopLimit)
    {
        Trace(0, "mm: usable RAM ends at 0x%p, past what one PDPT covers",
            ramEnd);
        ramEnd = TopLimit;
    }

    ulong blocks = 0;
    for (ulong block = LowMapLimit; block < ramEnd; block += BlockSize)
    {
        /* Skip a GiB with no RAM in it: mapping it write-back would license
           speculative reads of unbacked bus space for nothing. Everything
           usable below MappedLimit still ends up mapped, which is the
           property GetFreePages relies on. */
        if (!mmap.HasUsableRamIn(block, block + BlockSize))
            continue;

        auto& p3Entry = P3KernelPage.Entry[block / BlockSize];
        BugOn(p3Entry.Present());

        p3Entry.SetAddress(block);
        p3Entry.SetWritable();
        p3Entry.SetHuge();
        p3Entry.SetPresent();
        Hal::TlbFlushPage(PhysToVirt(block));

        blocks++;
    }

    if (blocks == 0)
        return;

    MappedLimit = ramEnd;
    Trace(0, "mm: bootstrap map extended to 0x%p with %u 1GiB blocks",
        MappedLimit, blocks);
}

}
}
