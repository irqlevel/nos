#pragma once

#include <include/types.h>

namespace Kernel
{

/* GICv3 driver: MMIO distributor + per-CPU redistributors, sysreg CPU
   interface (ICC_*). Affinity routing only (ARE=1), Group1NS interrupts,
   no LPI/ITS (deferred with PCIe MSI support). */
class Gic final
{
public:
    static Gic& GetInstance()
    {
        static Gic Instance;
        return Instance;
    }

    /* BSP once: map + init the distributor, then per-CPU init for self */
    bool Setup(ulong gicdPhys, ulong gicrPhys, ulong gicrSize);

    /* Per-CPU: wake own redistributor, enable SGIs/PPIs, init ICC_* */
    bool CpuInit();

    bool IsReady() const { return Ready; }

    /* Route an SPI to the CPU with the given MPIDR affinity and enable
       it; edge = true for edge-triggered. PPIs (16..31) are per-CPU:
       route is ignored, the enable lands in this CPU's redistributor. */
    void EnableIrq(u32 intId, ulong mpidr, bool edge);

    /* targetCpu is the kernel's linear CPU index; the MPIDR affinity is
       looked up in the Board CPU list (DTB order == linear index). */
    void SendSgi(ulong targetCpu, u32 intId);

    static u32 ReadIar();
    static void WriteEoir(u32 intId);

    static const u32 SpuriousIntId = 1020;

    /* Must match Hal::IpiVector (hal_irqchip_inline.h) */
    static const u32 IpiSgi = 1;

private:
    Gic() = default;
    ~Gic() = default;
    Gic(const Gic& other) = delete;
    Gic(Gic&& other) = delete;
    Gic& operator=(const Gic& other) = delete;
    Gic& operator=(Gic&& other) = delete;

    ulong RedistBaseForCpu();

    ulong GicdBase = 0;
    ulong GicrBase = 0;
    ulong GicrLimit = 0;
    bool Ready = false;

    /* Distributor registers */
    static const ulong GicdCtlr = 0x0000;
    static const ulong GicdTyper = 0x0004;
    static const ulong GicdIgroupr = 0x0080;
    static const ulong GicdIsenabler = 0x0100;
    static const ulong GicdIcenabler = 0x0180;
    static const ulong GicdIpriorityr = 0x0400;
    static const ulong GicdIcfgr = 0x0C00;
    static const ulong GicdIrouter = 0x6100;

    static const u32 CtlrEnableGrp1NS = 1 << 1;
    static const u32 CtlrAre = 1 << 4;
    static const u32 CtlrRwp = 1U << 31;

    /* Redistributor frames: RD_base + SGI_base, 64KiB each */
    static const ulong GicrTyper = 0x0008;
    static const ulong GicrWaker = 0x0014;
    static const ulong SgiOffset = 0x10000;
    static const ulong GicrIgroupr0 = 0x0080;
    static const ulong GicrIsenabler0 = 0x0100;
    static const ulong GicrIcenabler0 = 0x0180;
    static const ulong GicrIpriorityr = 0x0400;
    static const ulong GicrIcfgr1 = 0x0C04;
    static const ulong GicrStride = 0x20000;

    static const u32 WakerProcessorSleep = 1 << 1;
    static const u32 WakerChildrenAsleep = 1 << 2;
    static const u32 TyperLast = 1 << 4;

    static const u8 DefaultPriority = 0xA0;
};

}
