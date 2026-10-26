#pragma once

#include <include/types.h>

#include "fdt.h"

namespace kernel
{

/* Board description parsed once from the DTB at early boot (under the
   bootstrap linear map, before the page allocator exists). Hardcoded QEMU
   virt values remain as fallbacks behind "if (!found)" so bring-up does
   not depend on parser completeness.

   What the tree says is taken only as far as the kernel can act on it: a
   device's registers where the linear map reaches them (LinearMapReach), an
   interrupt the kernel can take (an INTID IsTakenIntId passes), bus numbers
   that are bus numbers. What fails that is refused -- left as if the tree
   had not said it, the fallback's to fill -- and counted in Refused, which
   the boot traces once there is a console to say it on. */
class Board final
{
public:
    static Board& get_instance()
    {
        static Board instance;
        return instance;
    }

    bool setup(void* dtb_va);

    struct Region
    {
        ulong addr;
        ulong size;
    };

    struct VirtioMmioDev
    {
        ulong base;
        ulong size;
        u32 int_id;
    };

    static const ulong MAX_MEM_REGIONS = 8;
    static const ulong MAX_VIRTIO_MMIO = 32;
    /* CPUs taken from the device tree. Kept at Kernel::MaxCpus so the FDT
       parser is not the thing that silently caps SMP. */
    static const ulong MAX_BOARD_CPUS = 64;

    ulong mem_region_count_ = 0;
    Region mem_regions_[MAX_MEM_REGIONS];

    Region dtb_region_ = {};

    char boot_args_[512] = {};

    bool psci_use_hvc_ = true; /* QEMU virt default conduit */

    ulong gicd_base_ = 0;
    ulong gicd_size_ = 0;
    ulong gicr_base_ = 0;
    ulong gicr_size_ = 0;

    ulong pl011_base_ = 0;
    u32 pl011_int_id_ = 0;

    ulong pl031_base_ = 0;
    u32 pl031_int_id_ = 0;

    u32 timer_int_id_ = 0; /* virtual timer PPI */

    /* GICv3 ITS (MSI). Zero base = no ITS found. */
    ulong its_base_ = 0;

    /* PCIe ECAM host bridge (pci-host-ecam-generic). Zero = no PCIe. */
    ulong ecam_base_ = 0;
    ulong ecam_size_ = 0;
    ulong pci_mmio32_base_ = 0;   /* CPU addr of the 32-bit non-prefetch window */
    ulong pci_mmio32_size_ = 0;
    ulong pci_mmio64_base_ = 0;   /* CPU addr of the 64-bit prefetch window */
    ulong pci_mmio64_size_ = 0;
    u8   pci_bus_start_ = 0;
    u8   pci_bus_end_ = 0;

    ulong virtio_mmio_count_ = 0;
    VirtioMmioDev virtio_mmio_[MAX_VIRTIO_MMIO];

    ulong cpu_count_ = 0;
    ulong cpu_mpidr_[MAX_BOARD_CPUS];

    /* Values of the tree's refused (see the class's comment) */
    ulong refused_ = 0;

    /* How far above physical 0 the kernel's linear map reaches: the most a
       physical address can be for KernelSpaceBase + it to be an address. */
    static const ulong LINEAR_MAP_REACH = 1UL << 47;

    /* The INTIDs the kernel can take: the GIC's PPIs and the SPIs below the
       256 its interrupt table holds (interrupt_arm64.cpp) and its
       registration's u8 carries. */
    static bool is_taken_int_id(u32 int_id);

private:
    Board() = default;
    ~Board() = default;
    Board(const Board& other) = delete;
    Board(Board&& other) = delete;
    Board& operator=(const Board& other) = delete;
    Board& operator=(Board&& other) = delete;

    void apply_fallbacks();

    /* [base, base + size) inside LinearMapReach; counts a refusal if not */
    bool reachable(u64 base, u64 size);

    /* A reg-like property's <address size> pair from cell index on, in the
       parent's cells, and Reachable; a pair it cannot read is a refusal too.
       False, and nothing counted, if there is no such property. */
    bool read_window(const Fdt::Prop& reg, ulong index, u32 ac, u32 sc, u64& base, u64& size);
};

}
