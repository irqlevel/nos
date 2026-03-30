#include "hpet.h"
#include "acpi.h"
#include "lapic.h"

#include <kernel/trace.h>
#include <kernel/cpu.h>
#include <kernel/interrupt.h>
#include <kernel/asm.h>
#include <mm/page_table.h>
#include <include/const.h>

namespace Kernel
{

Hpet::Hpet()
    : Mmio(nullptr)
    , PeriodFs(0)
    , FreqHz_(0)
    , TickPeriodNs(0)
    , IntVector(-1)
    , TimeNs(0)
{
}

Hpet::~Hpet()
{
}

u64 Hpet::ReadReg(ulong offset) volatile
{
    return *reinterpret_cast<volatile u64*>(reinterpret_cast<ulong>(Mmio) + offset);
}

void Hpet::WriteReg(ulong offset, u64 value) volatile
{
    *reinterpret_cast<volatile u64*>(reinterpret_cast<ulong>(Mmio) + offset) = value;
}

bool Hpet::Setup()
{
    ulong basePhys = Acpi::GetInstance().GetHpetBasePhys();
    if (basePhys == 0)
    {
        Trace(0, "HPET: no HPET ACPI table, using PIT");
        return false;
    }

    /* Map one page of HPET MMIO */
    ulong va = Mm::PageTable::GetInstance().MapMmioRegion(basePhys, Const::PageSize);
    if (va == 0)
    {
        Trace(0, "HPET: MapMmioRegion failed for phys 0x%p", basePhys);
        return false;
    }

    Mmio = reinterpret_cast<volatile u64*>(va);

    /* Read capabilities: period in femtoseconds is in bits 63:32 */
    u64 gcap = ReadReg(RegGcapId);
    PeriodFs = gcap >> GcapPeriodShift;
    ulong numTimers = (ulong)((gcap & GcapNumTimMask) >> GcapNumTimShift) + 1;

    if (PeriodFs == 0 || PeriodFs > 0x05F5E100ULL /* 100 ns max per spec */)
    {
        Trace(0, "HPET: invalid period %u fs", (ulong)PeriodFs);
        Mmio = nullptr;
        return false;
    }

    /* Frequency = 1e15 / period_fs */
    FreqHz_ = (ulong)(1000000000000000ULL / PeriodFs);

    /* Nanoseconds per tick at DesiredHz */
    TickPeriodNs = Const::NanoSecsInSec / DesiredHz;

    Trace(0, "HPET: phys 0x%p va 0x%p period %u fs freq %u Hz timers %u",
        basePhys, va, (ulong)PeriodFs, FreqHz_, numTimers);

    /* Disable counter while we configure */
    u64 conf = ReadReg(RegGenConf);
    conf &= ~ConfEnableCnf;
    WriteReg(RegGenConf, conf);

    /* Reset main counter to 0 */
    WriteReg(RegMainCnt, 0);

    /*
     * Configure timer 0 for periodic mode at DesiredHz.
     * Steps per HPET spec:
     *   1. Set INT_ENB_CNF, TYPE_CNF (periodic), VAL_SET_CNF
     *   2. Write the period (in counter ticks) to the comparator twice:
     *      first write sets the initial compare value;
     *      second write (with VAL_SET_CNF still set) sets the accumulator.
     */
    u64 ticksPerInterrupt = FreqHz_ / DesiredHz;
    u64 timerCfg = TimerIntEnb | TimerTypePer | TimerValSet;
    WriteReg(RegTimer0Cfg, timerCfg);
    WriteReg(RegTimer0Cmp, ticksPerInterrupt); /* initial comparator */
    WriteReg(RegTimer0Cmp, ticksPerInterrupt); /* periodic accumulator */

    /* Enable legacy replacement mode: timer 0 → IRQ 0 (replaces PIT),
       timer 1 → IRQ 8 (replaces RTC). This lets us register with the
       same GSI as the PIT, no explicit INT_ROUTE_CNF needed. */
    conf = ReadReg(RegGenConf);
    conf |= ConfEnableCnf | ConfLegRtCnf;
    WriteReg(RegGenConf, conf);

    Trace(0, "HPET: configured, ticksPerInt %u (100 Hz)", (ulong)ticksPerInterrupt);
    return true;
}

bool Hpet::IsAvailable()
{
    return Mmio != nullptr;
}

ulong Hpet::GetFreqHz()
{
    return FreqHz_;
}

Stdlib::Time Hpet::GetTime()
{
    if (Mmio == nullptr)
        return Stdlib::Time(0);

    u64 cnt = ReadReg(RegMainCnt);
    /* ns = cnt * PeriodFs / 1e6; use 128-bit intermediate to avoid overflow */
    u64 ns = (u64)((__uint128_t)cnt * PeriodFs / FemtoSecsPerNanoSec);
    return Stdlib::Time(ns);
}

void Hpet::Wait(ulong nanoSecs)
{
    Stdlib::Time expired = GetTime() + Stdlib::Time(nanoSecs);
    while (GetTime() < expired)
    {
        Pause();
    }
}

void Hpet::OnInterruptRegister(u8 irq, u8 vector)
{
    (void)irq;
    IntVector = vector;
}

InterruptHandlerFn Hpet::GetHandlerFn()
{
    return HpetInterruptStub;
}

void Hpet::Interrupt(Context* ctx)
{
    (void)ctx;

    /* Clear the interrupt status for timer 0 */
    WriteReg(RegGenIntSts, 1ULL);

    {
        TimeLock.WriteBegin();
        TimeNs = TimeNs + TickPeriodNs;
        TimeLock.WriteEnd();
    }

    CpuTable::GetInstance().SendIPIAll();
    Lapic::EOI(IntVector);
}

extern "C" void HpetInterrupt(Context* ctx)
{
    InterruptStats::Inc(IrqHpet);
    Hpet::GetInstance().Interrupt(ctx);
}

}
