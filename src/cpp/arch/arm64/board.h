#pragma once

#include <include/types.h>

namespace Kernel
{

/* Board description parsed once from the DTB at early boot (under the
   bootstrap linear map, before the page allocator exists). Hardcoded QEMU
   virt values remain as fallbacks behind "if (!found)" so bring-up does
   not depend on parser completeness. */
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
    static const ulong MaxBoardCpus = 8;

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

private:
    Board() = default;
    ~Board() = default;
    Board(const Board& other) = delete;
    Board(Board&& other) = delete;
    Board& operator=(const Board& other) = delete;
    Board& operator=(Board&& other) = delete;

    void ApplyFallbacks();
};

}
