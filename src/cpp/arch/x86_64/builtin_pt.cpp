#include <mm/page_table.h>
#include <mm/memory_map.h>

#include <kernel/trace.h>
#include <hal/mmu.h>

#include "cpuid.h"

namespace kernel
{

namespace mm
{

/* The x86 bootstrap linear map: defined in an arch TU so other
   architectures supply their own BuiltinPageTable::setup (see
   arch/arm64/builtin_pt.cpp). */

namespace
{

/* CPUID.80000001H:EDX[26] -- PDPE1GB: a PDPT entry may carry the page-size
   bit and map a 1GiB page on its own. Every x86-64 part since Barcelona and
   Nehalem has it; QEMU's default model does not advertise it, so the
   fallback below is a path that gets exercised rather than a theoretical
   one. */
bool cpu_has_giant_pages()
{
    static const u32 EXTENDED_LEAF_BASE = 0x80000000;
    static const u32 EXTENDED_FEATURE_LEAF = 0x80000001;
    static const u32 PDPE1_GB_BIT = 26;

    if (cpuid(EXTENDED_LEAF_BASE).eax < EXTENDED_FEATURE_LEAF)
        return false;

    return (cpuid(EXTENDED_FEATURE_LEAF).edx & (1U << PDPE1_GB_BIT)) != 0;
}

}

bool BuiltinPageTable::setup()
{
    /* The whole 4GB range is mapped write-back: MTRRs keep MMIO holes
       (LAPIC, IOAPIC, PCI) uncached on real hardware. Setting PCD here
       would run the entire kernel uncached on bare metal. */

    /* Four PDPT entries, each a page directory of 512 2MiB pages: 4GiB.
       That is the whole extent of the bootstrap map, and therefore the
       ceiling on the RAM the page allocator can take (see get_mapped_limit). */
    static const ulong KERNEL_MAP_SIZE = 4 * consts::GB;

    //Map first 4GB of kernel address space
    auto& p4_entry = p4_page_.entry[256];

    p4_entry.set_address(virt_to_phys((ulong)&p3_kernel_page_));
    p4_entry.set_writable();
    p4_entry.set_present();

    ulong addr = MemoryMap::KERNEL_SPACE_BASE;
    for (size_t i = 0; i < 4; i++)
    {
        auto& p3_entry = p3_kernel_page_.entry[i];
        auto& p2_page = p2_kernel_page_[i];

        p3_entry.set_address(virt_to_phys((ulong)&p2_page));
        p3_entry.set_writable();
        p3_entry.set_present();

        for (size_t j = 0; j < 512; j++)
        {
            auto& p2_entry = p2_page.entry[j];

            p2_entry.set_address(virt_to_phys(addr));
            p2_entry.set_writable();
            p2_entry.set_huge();
            p2_entry.set_present();
            hal::tlb_flush_page(phys_to_virt(addr));

            addr += (2 * consts::MB);
        }
    }

    mapped_limit_ = KERNEL_MAP_SIZE;

    //Map first 4GB of user address space

    auto& p4_entry2 = p4_page_.entry[0];

    p4_entry2.set_address(virt_to_phys((ulong)&p3_user_page_));
    p4_entry2.set_writable();
    p4_entry2.set_present();

    addr = 0;
    for (size_t i = 0; i < 4; i++)
    {
        auto& p3_entry = p3_user_page_.entry[i];
        auto& p2_page = p2_user_page_[i];

        p3_entry.set_address(virt_to_phys((ulong)&p2_page));
        p3_entry.set_writable();
        p3_entry.set_present();
        hal::tlb_flush_page((ulong)&p2_page);

        for (size_t j = 0; j < 512; j++)
        {
            auto& p2_entry = p2_page.entry[j];

            p2_entry.set_address(virt_to_phys(addr));
            p2_entry.set_writable();
            p2_entry.set_huge();
            p2_entry.set_present();
            hal::tlb_flush_page(phys_to_virt(addr));

            addr += (2 * consts::MB);
        }
    }

    p2_user_page_[0].entry[0].value = 0;
    hal::tlb_flush_page(0);

    return true;
}

void BuiltinPageTable::map_high_ram()
{
    /* The low 4GiB stays exactly as setup() built it, at 2MiB granularity.
       Below that line the MTRRs describe MMIO holes with memory types other
       than write-back, and a large page spanning two MTRR types is left to
       the implementation (SDM 11.11.9) -- in practice the most conservative
       type wins, which would make a whole GiB of RAM uncached. Above 4GiB
       the default MTRR type covers RAM, and only GiB blocks that carry RAM
       are mapped at all, so the mixed case needs a firmware that puts MMIO
       and RAM inside one GiB up there. Even then MTRRs still force the MMIO
       part uncached; the cost would be speed, not correctness. */
    static const ulong LOW_MAP_LIMIT = 4 * consts::GB;
    static const ulong BLOCK_BYTES = consts::GB;

    /* One PDPT entry per GiB, 512 of them under the kernel's P4 slot. */
    const ulong top_limit = stdlib::array_size(p3_kernel_page_.entry) * BLOCK_BYTES;

    BUG_ON(mapped_limit_ != LOW_MAP_LIMIT);

    auto& mmap = MemoryMap::get_instance();
    ulong ram_end = mmap.get_usable_ram_end();
    if (ram_end <= LOW_MAP_LIMIT)
        return;

    if (!cpu_has_giant_pages())
    {
        trace(0, "mm: no 1GiB page support, RAM above 0x%lX stays unusable",
            LOW_MAP_LIMIT);
        return;
    }

    if (ram_end > top_limit)
    {
        trace(0, "mm: usable RAM ends at 0x%lX, past what one PDPT covers",
            ram_end);
        ram_end = top_limit;
    }

    ulong blocks = 0;
    for (ulong block = LOW_MAP_LIMIT; block < ram_end; block += BLOCK_BYTES)
    {
        /* Skip a GiB with no RAM in it: mapping it write-back would license
           speculative reads of unbacked bus space for nothing. Everything
           usable below mapped_limit_ still ends up mapped, which is the
           property get_free_pages relies on. */
        if (!mmap.has_usable_ram_in(block, block + BLOCK_BYTES))
            continue;

        auto& p3_entry = p3_kernel_page_.entry[block / BLOCK_BYTES];
        BUG_ON(p3_entry.present());

        p3_entry.set_address(block);
        p3_entry.set_writable();
        p3_entry.set_huge();
        p3_entry.set_present();
        hal::tlb_flush_page(phys_to_virt(block));

        blocks++;
    }

    if (blocks == 0)
        return;

    mapped_limit_ = ram_end;
    trace(0, "mm: bootstrap map extended to 0x%lX with %lu 1GiB blocks",
        mapped_limit_, blocks);
}

}
}
