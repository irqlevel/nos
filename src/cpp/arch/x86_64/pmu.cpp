#include "pmu.h"
#include "asm.h"
#include "cpuid.h"
#include "lapic.h"

#include <kernel/trace.h>

namespace Kernel
{

namespace
{

/* SDM vol 3B, chapter 20. */
const u32 FixedCtr1Msr        = 0x30A;   /* unhalted core cycles */
const u32 FixedCtrCtrlMsr     = 0x38D;
const u32 GlobalStatusMsr     = 0x38E;
const u32 GlobalCtrlMsr       = 0x38F;
const u32 GlobalOvfCtrlMsr    = 0x390;
const u32 DebugCtlMsr         = 0x1D9;

/* IA32_DEBUGCTL.FREEZE_PERFMON_ON_PMI. Set, the counters stop dead on the
   first overflow and stay stopped; firmware and hypervisors do set it. This
   kernel rearms explicitly, so it wants the bit clear. */
const u64 FreezePerfmonOnPmi  = 1ULL << 12;

/* Fixed counter 1 occupies bits 4..7 of the control MSR: OS, USR, AnyThread,
   PMI. Kernel-only counting with an interrupt on overflow. */
const u64 FixedCtr1Enable     = (1ULL << 4) | (1ULL << 7);
const u64 FixedCtr1CtrlMask   = 0xFULL << 4;

/* Fixed counter i is bit 32+i in the global control and status MSRs. */
const u64 FixedCtr1GlobalBit  = 1ULL << 33;

const u32 IntelVendorEbx = 0x756E6547;   /* "Genu" */

u64 SavedPeriod;
u8 CounterWidth;

}

bool Pmu::Available()
{
    CpuidResult r0 = Cpuid(0);
    if (r0.Ebx != IntelVendorEbx)
        return false;

    if (r0.Eax < 0xA)
        return false;

    CpuidResult r = Cpuid(0xA);

    ulong version = r.Eax & 0xFF;
    ulong fixedCount = r.Edx & 0x1F;
    ulong fixedWidth = (r.Edx >> 5) & 0xFF;

    /* Version 2 is where the fixed counters and the global control MSRs
       arrive; counter 1 has to exist, and its width has to be sane. */
    if (version < 2 || fixedCount < 2 || fixedWidth == 0 || fixedWidth > 64)
        return false;

    CounterWidth = (u8)fixedWidth;
    return true;
}

u64 Pmu::CounterMask()
{
    if (CounterWidth >= 64)
        return ~0ULL;

    return (1ULL << CounterWidth) - 1;
}

bool Pmu::Start(ulong period)
{
    if (CounterWidth == 0 && !Available())
        return false;

    if (period < MinPeriod)
        period = MinPeriod;

    u64 mask = CounterMask();
    if ((u64)period >= mask)
        return false;

    SavedPeriod = period;

    WriteMsr(DebugCtlMsr, ReadMsr(DebugCtlMsr) & ~FreezePerfmonOnPmi);

    /* Off while it is being set up. */
    WriteMsr(GlobalCtrlMsr, ReadMsr(GlobalCtrlMsr) & ~FixedCtr1GlobalBit);
    WriteMsr(GlobalOvfCtrlMsr, FixedCtr1GlobalBit);

    u64 ctrl = ReadMsr(FixedCtrCtrlMsr) & ~FixedCtr1CtrlMask;
    WriteMsr(FixedCtrCtrlMsr, ctrl | FixedCtr1Enable);

    /* Count up to the wrap, so the overflow lands `period` cycles from now. */
    WriteMsr(FixedCtr1Msr, (mask - period) & mask);

    /* Deliver the overflow as an NMI: an ordinary vector would be blocked by
       exactly the interrupts-off code this is meant to be able to sample. */
    Lapic::WriteLvtPerfCounterNmi();

    WriteMsr(GlobalCtrlMsr, ReadMsr(GlobalCtrlMsr) | FixedCtr1GlobalBit);
    return true;
}

void Pmu::Stop()
{
    if (CounterWidth == 0)
        return;

    WriteMsr(GlobalCtrlMsr, ReadMsr(GlobalCtrlMsr) & ~FixedCtr1GlobalBit);
    WriteMsr(FixedCtrCtrlMsr, ReadMsr(FixedCtrCtrlMsr) & ~FixedCtr1CtrlMask);
    WriteMsr(GlobalOvfCtrlMsr, FixedCtr1GlobalBit);

    Lapic::MaskLvtPerfCounter();
}

bool Pmu::AckOverflow()
{
    /* SavedPeriod is 0 until some CPU has armed the counter. Rearming with it
       would set the counter one cycle short of its wrap, and the machine
       would never leave the NMI. */
    if (CounterWidth == 0 || SavedPeriod == 0)
        return false;

    if (!(ReadMsr(GlobalStatusMsr) & FixedCtr1GlobalBit))
        return false;

    u64 mask = CounterMask();

    /* Rearm before clearing: the counter is already counting again, and the
       order costs nothing. */
    WriteMsr(FixedCtr1Msr, (mask - SavedPeriod) & mask);
    WriteMsr(GlobalOvfCtrlMsr, FixedCtr1GlobalBit);

    /* Delivering through the local APIC's performance-counter entry sets its
       mask bit; without clearing it there is exactly one sample per run. */
    Lapic::WriteLvtPerfCounterNmi();

    /* Belt and braces against a machine that froze the counter on the PMI
       anyway: reasserting the enable costs one write and is the difference
       between a profile and a single sample. */
    WriteMsr(GlobalCtrlMsr, ReadMsr(GlobalCtrlMsr) | FixedCtr1GlobalBit);
    return true;
}

}
