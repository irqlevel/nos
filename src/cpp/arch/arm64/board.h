#pragma once

#include <include/types.h>

#include "fdt.h"

namespace Kernel
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
    static Board& GetInstance()
    {
        static Board Instance;
        return Instance;
    }

    bool Setup(void* dtbVa);

    struct Region
    {
        ulong Addr;
        ulong Size;
    };

    struct VirtioMmioDev
    {
        ulong Base;
        ulong Size;
        u32 IntId;
    };

    static const ulong MaxMemRegions = 8;
    static const ulong MaxVirtioMmio = 32;
    /* CPUs taken from the device tree. Kept at Kernel::MaxCpus so the FDT
       parser is not the thing that silently caps SMP. */
    static const ulong MaxBoardCpus = 64;

    ulong MemRegionCount = 0;
    Region MemRegions[MaxMemRegions];

    Region DtbRegion = {};

    char BootArgs[512] = {};

    bool PsciUseHvc = true; /* QEMU virt default conduit */

    ulong GicdBase = 0;
    ulong GicdSize = 0;
    ulong GicrBase = 0;
    ulong GicrSize = 0;

    ulong Pl011Base = 0;
    u32 Pl011IntId = 0;

    ulong Pl031Base = 0;
    u32 Pl031IntId = 0;

    u32 TimerIntId = 0; /* virtual timer PPI */

    /* GICv3 ITS (MSI). Zero base = no ITS found. */
    ulong ItsBase = 0;

    /* PCIe ECAM host bridge (pci-host-ecam-generic). Zero = no PCIe. */
    ulong EcamBase = 0;
    ulong EcamSize = 0;
    ulong PciMmio32Base = 0;   /* CPU addr of the 32-bit non-prefetch window */
    ulong PciMmio32Size = 0;
    ulong PciMmio64Base = 0;   /* CPU addr of the 64-bit prefetch window */
    ulong PciMmio64Size = 0;
    u8   PciBusStart = 0;
    u8   PciBusEnd = 0;

    ulong VirtioMmioCount = 0;
    VirtioMmioDev VirtioMmio[MaxVirtioMmio];

    ulong CpuCount = 0;
    ulong CpuMpidr[MaxBoardCpus];

    /* Values of the tree's refused (see the class's comment) */
    ulong Refused = 0;

    /* How far above physical 0 the kernel's linear map reaches: the most a
       physical address can be for KernelSpaceBase + it to be an address. */
    static const ulong LinearMapReach = 1UL << 47;

    /* The INTIDs the kernel can take: the GIC's PPIs and the SPIs below the
       256 its interrupt table holds (interrupt_arm64.cpp) and its
       registration's u8 carries. */
    static bool IsTakenIntId(u32 intId);

private:
    Board() = default;
    ~Board() = default;
    Board(const Board& other) = delete;
    Board(Board&& other) = delete;
    Board& operator=(const Board& other) = delete;
    Board& operator=(Board&& other) = delete;

    void ApplyFallbacks();

    /* [base, base + size) inside LinearMapReach; counts a refusal if not */
    bool Reachable(u64 base, u64 size);

    /* A reg-like property's <address size> pair from cell index on, in the
       parent's cells, and Reachable; a pair it cannot read is a refusal too.
       False, and nothing counted, if there is no such property. */
    bool ReadWindow(const Fdt::Prop& reg, ulong index, u32 ac, u32 sc, u64& base, u64& size);
};

}
