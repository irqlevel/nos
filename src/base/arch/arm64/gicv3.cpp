#include "gicv3.h"
#include "board.h"

#include <kernel/trace.h>
#include <kernel/panic.h>
#include <mm/page_table.h>
#include <mm/mmio.h>
#include <lib/stdlib.h>

namespace kernel
{

namespace
{

ulong read_mpidr()
{
    ulong mpidr;
    asm volatile("mrs %0, mpidr_el1" : "=r"(mpidr));
    return mpidr;
}

void gic_write32(ulong addr, u32 value)
{
    mm::MmIo::write32((void*)addr, value);
}

u32 gic_read32(ulong addr)
{
    return mm::MmIo::read32((void*)addr);
}

void gic_write64(ulong addr, u64 value)
{
    *reinterpret_cast<volatile u64*>(addr) = value;
}

}

bool Gic::setup(ulong gicd_phys, ulong gicr_phys, ulong gicr_size)
{
    auto& pt = mm::PageTable::get_instance();

    gicd_base_ = pt.map_mmio_region(gicd_phys, 0x10000);
    gicr_base_ = pt.map_mmio_region(gicr_phys, gicr_size);
    if (gicd_base_ == 0 || gicr_base_ == 0)
        return false;
    gicr_limit_ = gicr_base_ + gicr_size;

    /* Disable, then configure all SPIs to Group1NS with a default
       priority, everything masked. */
    gic_write32(gicd_base_ + GICD_CTLR, 0);
    while (gic_read32(gicd_base_ + GICD_CTLR) & CTLR_RWP)
    {
    }

    u32 typer = gic_read32(gicd_base_ + GICD_TYPER);
    u32 max_int_id = 32 * ((typer & 0x1F) + 1) - 1;
    if (max_int_id > 1019)
        max_int_id = 1019;

    for (u32 i = 32; i <= max_int_id; i += 32)
    {
        gic_write32(gicd_base_ + GICD_ICENABLER + (i / 32) * 4, ~0U);
        gic_write32(gicd_base_ + GICD_IGROUPR + (i / 32) * 4, ~0U);
    }
    for (u32 i = 32; i <= max_int_id; i += 4)
    {
        gic_write32(gicd_base_ + GICD_IPRIORITYR + i,
            ((u32)DEFAULT_PRIORITY << 24) | ((u32)DEFAULT_PRIORITY << 16) |
            ((u32)DEFAULT_PRIORITY << 8) | DEFAULT_PRIORITY);
    }

    gic_write32(gicd_base_ + GICD_CTLR, CTLR_ARE | CTLR_ENABLE_GRP1_NS);
    while (gic_read32(gicd_base_ + GICD_CTLR) & CTLR_RWP)
    {
    }

    trace(0, "Gic: gicd 0x%lX gicr 0x%lX max_int_id %lu", gicd_base_, gicr_base_,
        (ulong)max_int_id);

    if (!cpu_init())
        return false;

    ready_ = true;
    return true;
}

ulong Gic::redist_base_for_cpu()
{
    ulong aff = read_mpidr() & 0xFFFFFFULL; /* Aff2:Aff1:Aff0 */

    for (ulong base = gicr_base_; base < gicr_limit_; base += GICR_STRIDE)
    {
        u64 typer = gic_read32(base + GICR_TYPER) |
            ((u64)gic_read32(base + GICR_TYPER + 4) << 32);
        ulong typer_aff = (typer >> 32) & 0xFFFFFFULL;
        if (typer_aff == aff)
            return base;
        if (typer & TYPER_LAST)
            break;
    }
    return 0;
}

bool Gic::cpu_init()
{
    ulong rd = redist_base_for_cpu();
    if (rd == 0)
    {
        trace(0, "Gic: no redistributor for mpidr 0x%lX", read_mpidr());
        return false;
    }

    /* Wake the redistributor */
    u32 waker = gic_read32(rd + GICR_WAKER);
    waker &= ~WAKER_PROCESSOR_SLEEP;
    gic_write32(rd + GICR_WAKER, waker);
    while (gic_read32(rd + GICR_WAKER) & WAKER_CHILDREN_ASLEEP)
    {
    }

    /* SGIs + PPIs: Group1NS, default priority, all masked (enable_irq
       unmasks what gets registered) */
    ulong sgi = rd + SGI_OFFSET;
    gic_write32(sgi + GICR_ICENABLER0, ~0U);
    gic_write32(sgi + GICR_IGROUPR0, ~0U);
    for (ulong i = 0; i < 32; i += 4)
    {
        gic_write32(sgi + GICR_IPRIORITYR + i,
            ((u32)DEFAULT_PRIORITY << 24) | ((u32)DEFAULT_PRIORITY << 16) |
            ((u32)DEFAULT_PRIORITY << 8) | DEFAULT_PRIORITY);
    }

    /* The IPI SGI has no Interrupt::register_handler path (the dispatch loop
       special-cases it, like the x86 IDT slot in main.cpp) — enable it
       here for every CPU */
    gic_write32(sgi + GICR_ISENABLER0, 1U << IPI_SGI);

    /* CPU interface (sysregs) */
    ulong sre;
    asm volatile("mrs %0, icc_sre_el1" : "=r"(sre));
    asm volatile("msr icc_sre_el1, %0" :: "r"(sre | 1UL));
    asm volatile("isb");

    asm volatile("msr icc_pmr_el1, %0" :: "r"(0xF0UL));
    asm volatile("msr icc_bpr1_el1, %0" :: "r"(0UL));

    ulong ctlr;
    asm volatile("mrs %0, icc_ctlr_el1" : "=r"(ctlr));
    ctlr &= ~2UL; /* EOImode = 0: EOIR does priority drop + deactivate */
    asm volatile("msr icc_ctlr_el1, %0" :: "r"(ctlr));

    asm volatile("msr icc_igrpen1_el1, %0" :: "r"(1UL));
    asm volatile("isb");

    return true;
}

void Gic::enable_irq(u32 int_id, ulong mpidr, bool edge)
{
    BUG_ON(int_id >= 1020);

    if (int_id < 32)
    {
        /* SGI/PPI live in this CPU's redistributor; config for PPIs */
        ulong sgi = redist_base_for_cpu() + SGI_OFFSET;
        if (int_id >= 16)
        {
            u32 cfg = gic_read32(sgi + GICR_ICFGR1);
            u32 shift = (int_id - 16) * 2;
            cfg &= ~(3U << shift);
            if (edge)
                cfg |= 2U << shift;
            gic_write32(sgi + GICR_ICFGR1, cfg);
        }
        gic_write32(sgi + GICR_ISENABLER0, 1U << int_id);
        return;
    }

    /* SPI: trigger config, affinity route, enable */
    u32 cfg = gic_read32(gicd_base_ + GICD_ICFGR + (int_id / 16) * 4);
    u32 shift = (int_id % 16) * 2;
    cfg &= ~(3U << shift);
    if (edge)
        cfg |= 2U << shift;
    gic_write32(gicd_base_ + GICD_ICFGR + (int_id / 16) * 4, cfg);

    u64 route = mpidr & 0xFF00FFFFFFULL; /* Aff3:Aff2:Aff1:Aff0 */
    gic_write64(gicd_base_ + GICD_IROUTER + (int_id - 32) * 8, route);

    gic_write32(gicd_base_ + GICD_ISENABLER + (int_id / 32) * 4,
        1U << (int_id % 32));
}

void Gic::send_sgi(ulong target_cpu, u32 int_id)
{
    BUG_ON(int_id >= 16);

    auto& board = Board::get_instance();
    ulong mpidr = (target_cpu < board.cpu_count_)
        ? board.cpu_mpidr_[target_cpu]
        : target_cpu; /* pre-Board fallback: linear Aff0 (QEMU virt) */

    u64 aff0 = mpidr & 0xFF;
    u64 aff1 = (mpidr >> 8) & 0xFF;
    u64 aff2 = (mpidr >> 16) & 0xFF;
    u64 aff3 = (mpidr >> 32) & 0xFF;
    /* One ICC_SGI1R write targets a single 16-CPU Aff0 group */
    BUG_ON(aff0 >= 16);

    u64 sgir = (aff3 << 48) | (aff2 << 32) | (aff1 << 16) |
               ((u64)int_id << 24) | (1UL << aff0);

    /* Publish before kicking: stores queued for the target CPU (IPI work
       lists etc.) must be observable before the SGI arrives */
    asm volatile("dsb ishst" ::: "memory");
    asm volatile("msr icc_sgi1r_el1, %0" :: "r"(sgir));
    asm volatile("isb");
}

u32 Gic::read_iar()
{
    ulong iar;
    asm volatile("mrs %0, icc_iar1_el1" : "=r"(iar));
    return (u32)(iar & 0xFFFFFF);
}

void Gic::write_eoir(u32 int_id)
{
    asm volatile("msr icc_eoir1_el1, %0" :: "r"((ulong)int_id));
    asm volatile("isb");
}

}
