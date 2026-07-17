#include "gicv3.h"
#include "board.h"

#include <kernel/trace.h>
#include <kernel/panic.h>
#include <mm/page_table.h>
#include <mm/mmio.h>
#include <lib/stdlib.h>

namespace Kernel
{

namespace
{

ulong ReadMpidr()
{
    ulong mpidr;
    asm volatile("mrs %0, mpidr_el1" : "=r"(mpidr));
    return mpidr;
}

void GicWrite32(ulong addr, u32 value)
{
    Mm::MmIo::Write32((void*)addr, value);
}

u32 GicRead32(ulong addr)
{
    return Mm::MmIo::Read32((void*)addr);
}

void GicWrite64(ulong addr, u64 value)
{
    *reinterpret_cast<volatile u64*>(addr) = value;
}

}

bool Gic::Setup(ulong gicdPhys, ulong gicrPhys, ulong gicrSize)
{
    auto& pt = Mm::PageTable::GetInstance();

    GicdBase = pt.MapMmioRegion(gicdPhys, 0x10000);
    GicrBase = pt.MapMmioRegion(gicrPhys, gicrSize);
    if (GicdBase == 0 || GicrBase == 0)
        return false;
    GicrLimit = GicrBase + gicrSize;

    /* Disable, then configure all SPIs to Group1NS with a default
       priority, everything masked. */
    GicWrite32(GicdBase + GicdCtlr, 0);
    while (GicRead32(GicdBase + GicdCtlr) & CtlrRwp)
    {
    }

    u32 typer = GicRead32(GicdBase + GicdTyper);
    u32 maxIntId = 32 * ((typer & 0x1F) + 1) - 1;
    if (maxIntId > 1019)
        maxIntId = 1019;

    for (u32 i = 32; i <= maxIntId; i += 32)
    {
        GicWrite32(GicdBase + GicdIcenabler + (i / 32) * 4, ~0U);
        GicWrite32(GicdBase + GicdIgroupr + (i / 32) * 4, ~0U);
    }
    for (u32 i = 32; i <= maxIntId; i += 4)
    {
        GicWrite32(GicdBase + GicdIpriorityr + i,
            ((u32)DefaultPriority << 24) | ((u32)DefaultPriority << 16) |
            ((u32)DefaultPriority << 8) | DefaultPriority);
    }

    GicWrite32(GicdBase + GicdCtlr, CtlrAre | CtlrEnableGrp1NS);
    while (GicRead32(GicdBase + GicdCtlr) & CtlrRwp)
    {
    }

    Trace(0, "Gic: gicd 0x%p gicr 0x%p maxIntId %u", GicdBase, GicrBase,
        (ulong)maxIntId);

    if (!CpuInit())
        return false;

    Ready = true;
    return true;
}

ulong Gic::RedistBaseForCpu()
{
    ulong aff = ReadMpidr() & 0xFFFFFFULL; /* Aff2:Aff1:Aff0 */

    for (ulong base = GicrBase; base < GicrLimit; base += GicrStride)
    {
        u64 typer = GicRead32(base + GicrTyper) |
            ((u64)GicRead32(base + GicrTyper + 4) << 32);
        ulong typerAff = (typer >> 32) & 0xFFFFFFULL;
        if (typerAff == aff)
            return base;
        if (typer & TyperLast)
            break;
    }
    return 0;
}

bool Gic::CpuInit()
{
    ulong rd = RedistBaseForCpu();
    if (rd == 0)
    {
        Trace(0, "Gic: no redistributor for mpidr 0x%p", ReadMpidr());
        return false;
    }

    /* Wake the redistributor */
    u32 waker = GicRead32(rd + GicrWaker);
    waker &= ~WakerProcessorSleep;
    GicWrite32(rd + GicrWaker, waker);
    while (GicRead32(rd + GicrWaker) & WakerChildrenAsleep)
    {
    }

    /* SGIs + PPIs: Group1NS, default priority, all masked (EnableIrq
       unmasks what gets registered) */
    ulong sgi = rd + SgiOffset;
    GicWrite32(sgi + GicrIcenabler0, ~0U);
    GicWrite32(sgi + GicrIgroupr0, ~0U);
    for (ulong i = 0; i < 32; i += 4)
    {
        GicWrite32(sgi + GicrIpriorityr + i,
            ((u32)DefaultPriority << 24) | ((u32)DefaultPriority << 16) |
            ((u32)DefaultPriority << 8) | DefaultPriority);
    }

    /* The IPI SGI has no Interrupt::Register path (the dispatch loop
       special-cases it, like the x86 IDT slot in main.cpp) — enable it
       here for every CPU */
    GicWrite32(sgi + GicrIsenabler0, 1U << IpiSgi);

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

void Gic::EnableIrq(u32 intId, ulong mpidr, bool edge)
{
    BugOn(intId >= 1020);

    if (intId < 32)
    {
        /* SGI/PPI live in this CPU's redistributor; config for PPIs */
        ulong sgi = RedistBaseForCpu() + SgiOffset;
        if (intId >= 16)
        {
            u32 cfg = GicRead32(sgi + GicrIcfgr1);
            u32 shift = (intId - 16) * 2;
            cfg &= ~(3U << shift);
            if (edge)
                cfg |= 2U << shift;
            GicWrite32(sgi + GicrIcfgr1, cfg);
        }
        GicWrite32(sgi + GicrIsenabler0, 1U << intId);
        return;
    }

    /* SPI: trigger config, affinity route, enable */
    u32 cfg = GicRead32(GicdBase + GicdIcfgr + (intId / 16) * 4);
    u32 shift = (intId % 16) * 2;
    cfg &= ~(3U << shift);
    if (edge)
        cfg |= 2U << shift;
    GicWrite32(GicdBase + GicdIcfgr + (intId / 16) * 4, cfg);

    u64 route = mpidr & 0xFF00FFFFFFULL; /* Aff3:Aff2:Aff1:Aff0 */
    GicWrite64(GicdBase + GicdIrouter + (intId - 32) * 8, route);

    GicWrite32(GicdBase + GicdIsenabler + (intId / 32) * 4,
        1U << (intId % 32));
}

void Gic::SendSgi(ulong targetCpu, u32 intId)
{
    BugOn(intId >= 16);

    auto& board = Board::GetInstance();
    ulong mpidr = (targetCpu < board.CpuCount)
        ? board.CpuMpidr[targetCpu]
        : targetCpu; /* pre-Board fallback: linear Aff0 (QEMU virt) */

    u64 aff0 = mpidr & 0xFF;
    u64 aff1 = (mpidr >> 8) & 0xFF;
    u64 aff2 = (mpidr >> 16) & 0xFF;
    u64 aff3 = (mpidr >> 32) & 0xFF;
    /* One ICC_SGI1R write targets a single 16-CPU Aff0 group */
    BugOn(aff0 >= 16);

    u64 sgir = (aff3 << 48) | (aff2 << 32) | (aff1 << 16) |
               ((u64)intId << 24) | (1UL << aff0);

    /* Publish before kicking: stores queued for the target CPU (IPI work
       lists etc.) must be observable before the SGI arrives */
    asm volatile("dsb ishst" ::: "memory");
    asm volatile("msr icc_sgi1r_el1, %0" :: "r"(sgir));
    asm volatile("isb");
}

u32 Gic::ReadIar()
{
    ulong iar;
    asm volatile("mrs %0, icc_iar1_el1" : "=r"(iar));
    return (u32)(iar & 0xFFFFFF);
}

void Gic::WriteEoir(u32 intId)
{
    asm volatile("msr icc_eoir1_el1, %0" :: "r"((ulong)intId));
    asm volatile("isb");
}

}
