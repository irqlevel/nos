#include <mm/page_table.h>
#include <mm/memory_map.h>

#include <kernel/trace.h>
#include <hal/mmu.h>

#include "board.h"

namespace kernel
{

namespace mm
{

/* arm64 bootstrap linear map, the C++ twin of the tables boot.S builds:
   TTBR1 L0[256] -> L1[0..3] -> 512x2MiB L2 blocks covering the first 4GiB
   at KernelSpaceBase. Same 9-9-9-9-12 geometry as x86, so the same class
   layout (P4Page = L0, P3KernelPage = L1, P2KernelPage = L2) is reused.
   Differences from x86: physical ranges below the RAM base are mapped
   Device-nGnRE (no MTRRs to fix cacheability), ranges past the end of RAM
   are left unmapped (a Normal-cacheable mapping would license speculative
   reads of unbacked bus space), and there is no low-half identity map —
   TTBR0 walks are disabled for good via TCR.EPD0 (boot.S). */

/* All QEMU-virt MMIO sits below the RAM base and is covered by the 1GiB
   Device-nGnRE L1 block InstallEarlyDeviceBlock puts into the real table. */
ulong early_device_limit;

bool BuiltinPageTable::setup()
{
    auto& board = Board::get_instance();
    ulong ram_base = board.mem_regions_[0].addr;
    ulong ram_end = ram_base;
    for (ulong i = 0; i < board.mem_region_count_; i++)
    {
        ulong end = board.mem_regions_[i].addr + board.mem_regions_[i].size;
        if (end > ram_end)
            ram_end = end;
    }

    auto& l0_entry = p4_page_.entry[256];

    l0_entry.set_address(virt_to_phys((ulong)&p3_kernel_page_));
    l0_entry.set_writable();
    l0_entry.set_present();

    ulong addr = MemoryMap::KERNEL_SPACE_BASE;
    for (size_t i = 0; i < 4; i++)
    {
        auto& l1_entry = p3_kernel_page_.entry[i];
        auto& l2_page = p2_kernel_page_[i];

        l1_entry.set_address(virt_to_phys((ulong)&l2_page));
        l1_entry.set_writable();
        l1_entry.set_present();

        for (size_t j = 0; j < 512; j++)
        {
            auto& l2_entry = l2_page.entry[j];
            ulong phys = virt_to_phys(addr);

            /* Past the end of RAM there is nothing to map */
            if (phys >= ram_end && phys >= ram_base)
            {
                addr += (2 * consts::MB);
                continue;
            }

            l2_entry.set_address(phys);
            l2_entry.set_writable();
            l2_entry.set_huge();
            l2_entry.set_present();
            if (phys < ram_base)
                l2_entry.set_cache_disabled();

            addr += (2 * consts::MB);
        }
    }

    /* Four L1 entries of 512 2MiB blocks: 4GiB of physical space, whatever
       part of it is RAM. The ceiling on what the page allocator can take
       (see GetMappedLimit). */
    mapped_limit_ = 4 * consts::GB;

    return true;
}

void BuiltinPageTable::map_high_ram()
{
    /* The first 4GiB is what Setup()'s four L2 tables cover. Past that one
       L1 block per GiB is enough, and with a 4KiB granule an L1 block
       descriptor is architectural -- no feature bit to test, unlike x86. */
    static const ulong LOW_MAP_LIMIT = 4 * consts::GB;
    static const ulong BLOCK_BYTES = consts::GB;

    const ulong top_limit = stdlib::array_size(p3_kernel_page_.entry) * BLOCK_BYTES;

    BUG_ON(mapped_limit_ != LOW_MAP_LIMIT);

    auto& mmap = MemoryMap::get_instance();
    ulong ram_end = mmap.get_usable_ram_end();
    if (ram_end <= LOW_MAP_LIMIT)
        return;

    if (ram_end > top_limit)
    {
        trace(0, "mm: usable RAM ends at 0x%lX, past what one L1 table covers",
            ram_end);
        ram_end = top_limit;
    }

    ulong blocks = 0;
    for (ulong block = LOW_MAP_LIMIT; block < ram_end; block += BLOCK_BYTES)
    {
        /* Normal-cacheable over a GiB with no RAM in it would license
           speculative reads of unbacked bus space -- the same reason Setup()
           leaves the space past ramEnd alone. */
        if (!mmap.has_usable_ram_in(block, block + BLOCK_BYTES))
            continue;

        auto& l1_entry = p3_kernel_page_.entry[block / BLOCK_BYTES];
        BUG_ON(l1_entry.present());

        l1_entry.set_address(block);
        l1_entry.set_writable();
        l1_entry.set_huge();
        l1_entry.set_present();
        hal::tlb_flush_page(phys_to_virt(block));

        blocks++;
    }

    if (blocks == 0)
        return;

    mapped_limit_ = ram_end;
    trace(0, "mm: bootstrap map extended to 0x%lX with %lu 1GiB blocks",
        mapped_limit_, blocks);
}

/* Install one 1GiB Device-nGnRE L1 block covering phys [0, ramBase-ish)
   into the freshly built real page table, so PL011/GIC/virtio survive the
   builtin->real root switch (x86 has no such need: its console is port
   I/O and MMIO regions are mapped later via MapMmioRegion). Must run
   BEFORE the switch: the real table's pages are edited through the still-
   active builtin linear map. */
bool install_early_device_block(ulong real_root)
{
    auto& bpt = BuiltinPageTable::get_instance();

    PtePage* l0 = (PtePage*)bpt.phys_to_virt(real_root);
    Pte* l0_entry = &l0->entry[Pte::l4_index(MemoryMap::KERNEL_SPACE_BASE)];
    if (!l0_entry->present())
        return false;

    PtePage* l1 = (PtePage*)bpt.phys_to_virt(l0_entry->address());
    Pte* l1_entry = &l1->entry[0]; /* [KernelSpaceBase, +1GiB): all MMIO */
    if (l1_entry->present())
        return false;

    l1_entry->set_address(0);
    l1_entry->set_writable();
    l1_entry->set_huge();
    l1_entry->set_cache_disabled();
    l1_entry->set_present();

    early_device_limit = 1 * consts::GB;
    return true;
}

}
}

namespace hal
{

ulong mmio_premapped_va(ulong phys_addr, ulong size_bytes)
{
    using kernel::mm::MemoryMap;
    if (kernel::mm::early_device_limit != 0 &&
        phys_addr + size_bytes <= kernel::mm::early_device_limit)
        return MemoryMap::KERNEL_SPACE_BASE + phys_addr;
    return 0;
}

}
