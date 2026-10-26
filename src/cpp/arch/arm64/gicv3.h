#pragma once

#include <include/types.h>

namespace kernel
{

/* GICv3 driver: MMIO distributor + per-CPU redistributors, sysreg CPU
   interface (ICC_*). Affinity routing only (ARE=1), Group1NS interrupts,
   no LPI/ITS (deferred with PCIe MSI support). */
class Gic final
{
public:
    static Gic& get_instance()
    {
        static Gic instance;
        return instance;
    }

    /* BSP once: map + init the distributor, then per-CPU init for self */
    bool setup(ulong gicd_phys, ulong gicr_phys, ulong gicr_size);

    /* Per-CPU: wake own redistributor, enable SGIs/PPIs, init ICC_* */
    bool cpu_init();

    bool is_ready() const { return ready_; }

    /* Route an SPI to the CPU with the given MPIDR affinity and enable
       it; edge = true for edge-triggered. PPIs (16..31) are per-CPU:
       route is ignored, the enable lands in this CPU's redistributor. */
    void enable_irq(u32 int_id, ulong mpidr, bool edge);

    /* target_cpu is the kernel's linear CPU index; the MPIDR affinity is
       looked up in the Board CPU list (DTB order == linear index). */
    void send_sgi(ulong target_cpu, u32 int_id);

    static u32 read_iar();
    static void write_eoir(u32 int_id);

    static const u32 SPURIOUS_INT_ID = 1020;

    /* Must match hal::IPI_VECTOR (hal_irqchip_inline.h) */
    static const u32 IPI_SGI = 1;

private:
    Gic() = default;
    ~Gic() = default;
    Gic(const Gic& other) = delete;
    Gic(Gic&& other) = delete;
    Gic& operator=(const Gic& other) = delete;
    Gic& operator=(Gic&& other) = delete;

    ulong redist_base_for_cpu();

    ulong gicd_base_ = 0;
    ulong gicr_base_ = 0;
    ulong gicr_limit_ = 0;
    bool ready_ = false;

    /* Distributor registers */
    static const ulong GICD_CTLR = 0x0000;
    static const ulong GICD_TYPER = 0x0004;
    static const ulong GICD_IGROUPR = 0x0080;
    static const ulong GICD_ISENABLER = 0x0100;
    static const ulong GICD_ICENABLER = 0x0180;
    static const ulong GICD_IPRIORITYR = 0x0400;
    static const ulong GICD_ICFGR = 0x0C00;
    static const ulong GICD_IROUTER = 0x6100;

    static const u32 CTLR_ENABLE_GRP1_NS = 1 << 1;
    static const u32 CTLR_ARE = 1 << 4;
    static const u32 CTLR_RWP = 1U << 31;

    /* Redistributor frames: RD_base + SGI_base, 64KiB each */
    static const ulong GICR_TYPER = 0x0008;
    static const ulong GICR_WAKER = 0x0014;
    static const ulong SGI_OFFSET = 0x10000;
    static const ulong GICR_IGROUPR0 = 0x0080;
    static const ulong GICR_ISENABLER0 = 0x0100;
    static const ulong GICR_ICENABLER0 = 0x0180;
    static const ulong GICR_IPRIORITYR = 0x0400;
    static const ulong GICR_ICFGR1 = 0x0C04;
    static const ulong GICR_STRIDE = 0x20000;

    static const u32 WAKER_PROCESSOR_SLEEP = 1 << 1;
    static const u32 WAKER_CHILDREN_ASLEEP = 1 << 2;
    static const u32 TYPER_LAST = 1 << 4;

    static const u8 DEFAULT_PRIORITY = 0xA0;
};

}
