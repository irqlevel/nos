#include "pmu.h"
#include "asm.h"
#include "cpuid.h"
#include "lapic.h"

#include <hal/barrier.h>
#include <hal/cpu.h>
#include <kernel/cpu.h>

namespace Kernel
{

namespace
{

/* A counter that is really counting moves within one round of PAUSEs -- tens
   of thousands of cycles, tens of microseconds. The extra rounds are for a
   counter that is slow to be believed rather than dead, and only a dead one
   ever pays for all of them. */
const ulong SelfTestSpins = 256;
const ulong SelfTestRounds = 4;

/* Does this MSR count? Interrupts are already off, so the reads bracket a
   spin on one CPU rather than two. */
bool MsrMoves(u32 msr, u64 mask)
{
    u64 before = ReadMsr(msr) & mask;

    for (ulong round = 0; round < SelfTestRounds; round++)
    {
        for (ulong i = 0; i < SelfTestSpins; i++)
            Pause();

        if ((ReadMsr(msr) & mask) != before)
            return true;
    }

    return false;
}

/* Which performance-counter interface this part speaks. Decided once, by
   CPUID, on the CPU that starts the profiler. */
enum Flavor
{
    FlavorNone = 0,
    FlavorIntel,
    FlavorAmd,
};

/* Read by every CPU that arms a counter, written once by the CPU that
   probes. Volatile so the two stores below cannot be reordered against each
   other, which is what makes Probed a safe flag to publish Kind behind. */
volatile Flavor Kind = FlavorNone;
volatile bool Probed;

/* Written by every CPU that arms its counter, always with the same value:
   the profiler asks for one period and hands it to all of them. */
u64 SavedPeriod;

/* Intel: architectural perfmon, fixed counter 1. SDM vol 3B, chapter 20. */
namespace Intel
{

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

u8 CounterWidth;

u64 CounterMask()
{
    if (CounterWidth >= 64)
        return ~0ULL;

    return (1ULL << CounterWidth) - 1;
}

bool Detect()
{
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

bool Start(ulong period)
{
    u64 mask = CounterMask();
    if ((u64)period >= mask)
        return false;

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

void Stop()
{
    WriteMsr(GlobalCtrlMsr, ReadMsr(GlobalCtrlMsr) & ~FixedCtr1GlobalBit);
    WriteMsr(FixedCtrCtrlMsr, ReadMsr(FixedCtrCtrlMsr) & ~FixedCtr1CtrlMask);
    WriteMsr(GlobalOvfCtrlMsr, FixedCtr1GlobalBit);

    Lapic::MaskLvtPerfCounter();
}

bool CountsForReal()
{
    /* Enabled without the PMI bit: a probe this short cannot overflow, and an
       interrupt out of one would arrive with no profiler behind it. */
    WriteMsr(GlobalCtrlMsr, ReadMsr(GlobalCtrlMsr) & ~FixedCtr1GlobalBit);
    WriteMsr(FixedCtrCtrlMsr,
        (ReadMsr(FixedCtrCtrlMsr) & ~FixedCtr1CtrlMask) | (1ULL << 4));
    WriteMsr(FixedCtr1Msr, 0);
    WriteMsr(GlobalCtrlMsr, ReadMsr(GlobalCtrlMsr) | FixedCtr1GlobalBit);

    bool counts = MsrMoves(FixedCtr1Msr, CounterMask());

    Stop();
    return counts;
}

bool AckOverflow()
{
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

/* AMD: general counter 0, event PMCx076. PPR for family 17h/19h, section
   "Performance Monitor Counters". */
namespace Amd
{

/* Core::X86::Msr::PERF_CTL/PERF_CTR -- the six-counter core extension,
   family 15h and later, interleaved control and counter two MSRs apart.

   AMD also has four legacy counters at 0xC0010000, going back to the K7 and
   present on every part here as aliases of the first four of these. They are
   deliberately not used: no CPUID bit says they exist, so there is nothing
   to check before writing them, and a guest whose host has turned the
   virtual PMU off takes a #GP -- which in this kernel is a panic -- from the
   first write. PerfCtrExtCore is a bit that a hypervisor with no PMU to give
   clears, and KVM does. Every MSR touched below is behind either it or
   PerfMonV2. */
const u32 Ctl0Msr = 0xC0010200;
const u32 Ctr0Msr = 0xC0010201;

/* PerfMonV2 (Zen 4 and later): global control and status, Intel-shaped.
   Counter 0 is bit 0 in all three. */
const u32 GlobalStatusMsr    = 0xC0010300;
const u32 GlobalCtlMsr       = 0xC0010301;
const u32 GlobalStatusClrMsr = 0xC0010302;
const u64 GlobalCtr0Bit      = 1ULL << 0;

/* PMCx076, "CPU Clocks not Halted": the same thing Intel's fixed counter 1
   counts, and the one AMD event number that has not moved between families
   since the K8. */
const u64 EventCyclesNotHalted = 0x76;

/* PERF_CTL: EventSelect[7:0] at 0, UnitMask at 8, USR 16, OS 17, INT 20
   (raise the local APIC's performance-counter interrupt on overflow), EN 22,
   EventSelect[11:8] at 32. Kernel-only, interrupt on overflow -- USR is left
   clear for the same reason Intel's is. */
const u64 CtlOs   = 1ULL << 17;
const u64 CtlInt  = 1ULL << 20;
const u64 CtlEn   = 1ULL << 22;

/* Every AMD performance counter is 48 bits wide, and no CPUID field says so
   -- unlike Intel, where the width is enumerated. */
const ulong CounterWidth = 48;
const u64 CounterMask = (1ULL << CounterWidth) - 1;
const u64 CounterSignBit = 1ULL << (CounterWidth - 1);

const u32 PerfCtrExtCoreBit = 1u << 23;   /* CPUID Fn8000_0001_ECX */
const u32 PerfMonV2Bit      = 1u << 0;    /* CPUID Fn8000_0022_EAX */

bool V2;

bool Detect()
{
    u32 maxExtended = Cpuid(0x80000000).Eax;
    if (maxExtended < 0x80000001)
        return false;

    /* The one gate on the counter MSRs. It also happens to exclude QEMU's
       default `qemu64`, which calls itself an AuthenticAMD family 0Fh part
       with no performance counters behind the claim. */
    if (!(Cpuid(0x80000001).Ecx & PerfCtrExtCoreBit))
        return false;

    V2 = false;
    if (maxExtended >= 0x80000022)
    {
        CpuidResult r22 = Cpuid(0x80000022);

        /* Fn8000_0022_EBX[3:0] is the number of core counters. A part that
           claims PerfMonV2 and no counters is one to walk away from rather
           than to write a global enable bit for. */
        if ((r22.Eax & PerfMonV2Bit) != 0)
        {
            if ((r22.Ebx & 0xF) == 0)
                return false;

            V2 = true;
        }
    }

    return true;
}

bool Start(ulong period)
{
    /* The counter is armed below zero, so the period has to fit in the
       counter with its sign bit to spare. */
    if ((u64)period >= CounterSignBit)
        return false;

    /* Off while it is being set up. Writing the whole control register
       clears the enable along with any event a previous owner left there. */
    WriteMsr(Ctl0Msr, 0);

    if (V2)
    {
        WriteMsr(GlobalCtlMsr, ReadMsr(GlobalCtlMsr) & ~GlobalCtr0Bit);
        WriteMsr(GlobalStatusClrMsr, GlobalCtr0Bit);
    }

    /* Count up to the wrap from `period` cycles below it. On a part without
       the global status register this negative start is also how the
       overflow is recognised later: the sign bit is set now and clear once
       the counter has passed zero. */
    WriteMsr(Ctr0Msr, (0 - (u64)period) & CounterMask);

    /* Deliver the overflow as an NMI, for the same reason Intel does: an
       ordinary vector would be blocked by exactly the interrupts-off code
       this is meant to be able to sample. The local APIC entry is the same
       one on both vendors -- only the counter differs. */
    Lapic::WriteLvtPerfCounterNmi();

    WriteMsr(Ctl0Msr, EventCyclesNotHalted | CtlOs | CtlInt | CtlEn);

    /* On a PerfMonV2 part the per-counter enable is not enough on its own:
       the global bit gates it, and it resets to zero. Writing this MSR on a
       part without PerfMonV2 would be a #GP, hence the flag. */
    if (V2)
        WriteMsr(GlobalCtlMsr, ReadMsr(GlobalCtlMsr) | GlobalCtr0Bit);

    return true;
}

void Stop()
{
    if (V2)
    {
        WriteMsr(GlobalCtlMsr, ReadMsr(GlobalCtlMsr) & ~GlobalCtr0Bit);
        WriteMsr(GlobalStatusClrMsr, GlobalCtr0Bit);
    }

    WriteMsr(Ctl0Msr, 0);

    Lapic::MaskLvtPerfCounter();
}

bool CountsForReal()
{
    /* Counting from zero with the interrupt bit clear: nothing here can
       overflow, and nothing here can raise an NMI with no profiler behind
       it. */
    WriteMsr(Ctl0Msr, 0);
    if (V2)
    {
        WriteMsr(GlobalCtlMsr, ReadMsr(GlobalCtlMsr) & ~GlobalCtr0Bit);
        WriteMsr(GlobalStatusClrMsr, GlobalCtr0Bit);
    }

    WriteMsr(Ctr0Msr, 0);
    WriteMsr(Ctl0Msr, EventCyclesNotHalted | CtlOs | CtlEn);

    if (V2)
        WriteMsr(GlobalCtlMsr, ReadMsr(GlobalCtlMsr) | GlobalCtr0Bit);

    bool counts = MsrMoves(Ctr0Msr, CounterMask);

    Stop();
    return counts;
}

bool AckOverflow()
{
    if (V2)
    {
        if (!(ReadMsr(GlobalStatusMsr) & GlobalCtr0Bit))
            return false;
    }
    else
    {
        /* No global status register to ask. The counter itself answers: it
           was armed below zero, so its sign bit is set while it is still
           counting up to the overflow and clear once it has gone past. The
           counter keeps running through the overflow on AMD, so a handful
           of cycles have already accumulated past the wrap -- they are lost
           in the rearm, and against a three-million-cycle period they do
           not move the sampling rate. */
        if (ReadMsr(Ctr0Msr) & CounterSignBit)
            return false;
    }

    WriteMsr(Ctr0Msr, (0 - SavedPeriod) & CounterMask);

    if (V2)
        WriteMsr(GlobalStatusClrMsr, GlobalCtr0Bit);

    /* Harmless on a part that does not mask its performance-counter entry on
       delivery, and the difference between a profile and one sample on a
       part that does. */
    Lapic::WriteLvtPerfCounterNmi();
    return true;
}

}

/* AMD delivers the occasional performance-counter NMI after the overflow that
   caused it has already been handled -- the interrupt was in flight while the
   handler was reading the counter. Linux allows one such NMI per handled
   overflow, and so does this: a credit per CPU, set when a sample is taken
   and spent by the next NMI that no counter claims. An NMI beyond the credit
   still reaches the panic, which is what a genuine one has to do.

   Indexed by hardware CPU id, like the profiler's own per-CPU buffers, and
   written only by its own CPU from its own NMI. */
volatile u8 SpuriousCredit[MaxCpus];

/* Counted rather than silent: a credit spent after the profiler has stopped
   is an NMI this kernel swallowed, and `lscpu` says how many. Per CPU, like
   everything else an NMI writes here, so counting one costs no atomic and
   loses nothing to a simultaneous NMI on another core. */
ulong SpuriousNmis[MaxCpus];

/* Whether this CPU currently has its counter armed. Load-bearing on AMD
   without PerfMonV2, where an overflow is recognised by the sign bit of a
   counter that keeps whatever value it stopped at: once the profiler has
   run, that bit is as likely clear as not, and without this flag the next
   genuine NMI -- arriving minutes later, from something else entirely --
   would be read as a sample and swallowed instead of panicking. */
volatile u8 Armed[MaxCpus];

}

bool Pmu::Available()
{
    if (Probed)
    {
        /* Pairs with the publish below: Kind and everything Detect() left
           behind it must be visible to a CPU that has seen Probed. */
        Hal::SmpRmb();
        return Kind != FlavorNone;
    }

    /* Interrupts off for the whole probe. It runs in task context, where
       this task can otherwise be preempted and resumed on another CPU
       between arming the counter and reading it back -- and then the two
       reads bracketing the spin come off two different CPUs' counters, which
       answers a question nobody asked. */
    ulong flags = Hal::IrqSave();

    Flavor kind = FlavorNone;

    CpuidResult r0 = Cpuid(0);

    static const u32 IntelVendorEbx = 0x756E6547;   /* "Genu" */
    static const u32 AmdVendorEbx   = 0x68747541;   /* "Auth" */

    if (r0.Ebx == IntelVendorEbx)
    {
        if (r0.Eax >= 0xA && Intel::Detect())
            kind = FlavorIntel;
    }
    else if (r0.Ebx == AmdVendorEbx)
    {
        if (Amd::Detect())
            kind = FlavorAmd;
    }

    /* CPUID has said the counter is there. Now make it count: under a
       hypervisor that does not virtualise the PMU the writes are accepted
       and the counter never moves, and a profiler that trusted CPUID would
       arm it on every CPU and report an empty profile with no hint as to
       why. At most four short spins, once. */
    if (kind == FlavorIntel && !Intel::CountsForReal())
        kind = FlavorNone;
    else if (kind == FlavorAmd && !Amd::CountsForReal())
        kind = FlavorNone;

    /* Published after the answer is complete, and after a barrier: the CPU
       running the shell gets here first, and the CPUs that go on to arm their
       own counters read Kind, CounterWidth and V2 on the strength of having
       seen Probed. */
    Kind = kind;
    Hal::SmpWmb();
    Probed = true;

    Hal::IrqRestore(flags);
    return kind != FlavorNone;
}

bool Pmu::Start(ulong period)
{
    /* Available(), not Probed, is what decides whether there is a counter --
       but this runs from the tick, in an interrupt, and Available() spins for
       tens of microseconds the first time it is asked. The profiler calls it
       from the shell before it sets a single CPU sampling, so by the time any
       tick gets here the answer is already in. If it is not, this tick stays
       on the tick. */
    if (!Probed)
        return false;

    Hal::SmpRmb();
    if (Kind == FlavorNone)
        return false;

    /* A CPU with no slot in the armed table would take overflows that
       AckOverflow() then refuses to claim, and every one of them would reach
       the panic. Leave that CPU on the tick instead. */
    ulong index = Hal::GetCurrentCpuHwId();
    if (index >= MaxCpus)
        return false;

    if (period < MinPeriod)
        period = MinPeriod;

    SavedPeriod = period;

    /* Armed before the counter is, not after: an overflow landing between the
       two is this profiler's, and the flag is what says so. */
    Armed[index] = 1;

    if (!((Kind == FlavorIntel) ? Intel::Start(period) : Amd::Start(period)))
    {
        Armed[index] = 0;
        return false;
    }

    return true;
}

void Pmu::Stop()
{
    if (Kind == FlavorIntel)
        Intel::Stop();
    else if (Kind == FlavorAmd)
        Amd::Stop();

    /* After the counter is off, not before: an overflow in between is still
       this profiler's, and one that arrives after is a stopped counter's
       leftover value, which is exactly what the flag exists to disregard. */
    ulong index = Hal::GetCurrentCpuHwId();
    if (index < MaxCpus)
        Armed[index] = 0;

    /* The spurious credit is deliberately left standing across the stop. The
       in-flight NMI that it exists for can arrive after the counter is
       disarmed, and swallowing one stray NMI at the end of a profiling run
       is a better trade than panicking on it. */
}

bool Pmu::AckOverflow()
{
    /* SavedPeriod is 0 until some CPU has armed the counter. Rearming with it
       would set the counter one cycle short of its wrap, and the machine
       would never leave the NMI. */
    if (Kind == FlavorNone || SavedPeriod == 0)
        return false;

    ulong index = Hal::GetCurrentCpuHwId();
    if (index >= MaxCpus || !Armed[index])
        return false;

    if (!((Kind == FlavorIntel) ? Intel::AckOverflow() : Amd::AckOverflow()))
        return false;

    SpuriousCredit[index] = 1;
    return true;
}

bool Pmu::AbsorbSpuriousNmi()
{
    if (Kind != FlavorAmd)
        return false;

    ulong index = Hal::GetCurrentCpuHwId();
    if (index >= MaxCpus || SpuriousCredit[index] == 0)
        return false;

    SpuriousCredit[index] = 0;

    /* Counted, not traced: Trace takes the dmesg lock, and this runs in an
       NMI that may have landed on a CPU already holding it. `lscpu` reads
       the count where printing it is safe. */
    SpuriousNmis[index]++;
    return true;
}

ulong Pmu::SpuriousNmiCount()
{
    ulong total = 0;
    for (ulong i = 0; i < MaxCpus; i++)
        total += SpuriousNmis[i];

    return total;
}

const char* Pmu::Name()
{
    switch (Kind)
    {
    case FlavorIntel:
        return "pmu/nmi intel fixed-ctr1";
    case FlavorAmd:
        return Amd::V2 ? "pmu/nmi amd pmc0 v2" : "pmu/nmi amd pmc0";
    default:
        return "pmu/nmi";
    }
}

}
