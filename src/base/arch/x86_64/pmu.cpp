#include "pmu.h"
#include "asm.h"
#include "cpuid.h"
#include "lapic.h"

#include <hal/barrier.h>
#include <hal/cpu.h>
#include <kernel/cpu.h>

namespace kernel
{

namespace
{

/* How long the probe below may spend before giving up on a counter, in core
   cycles -- about ten microseconds on a 3 GHz part.

   Bounded by the cycle counter rather than by a count of PAUSEs on purpose:
   PAUSE is ten cycles on one part and a hundred and forty on the next, so a
   fixed number of them is a duration nobody can state -- and this is time
   spent with interrupts off. Ten microseconds is one interrupt handler, paid
   once per boot.

   It is also far more than the answer needs. A counter that is running
   advances every unhalted cycle, and one RDMSR alone is a hundred cycles on
   bare metal and a VM exit under a hypervisor, so a live counter has already
   moved by the first check and the loop returns there. Only a counter that
   is not really there spends the whole budget. */
const ulong SELF_TEST_CYCLES = 30000;

/* Does this MSR count? Interrupts are already off, so both reads come off
   one CPU's counter rather than two. */
bool msr_moves(u32 msr, u64 mask)
{
    u64 before = read_msr(msr) & mask;
    u64 deadline = hal::read_cycle_counter() + SELF_TEST_CYCLES;

    do
    {
        if ((read_msr(msr) & mask) != before)
            return true;

        cpu_relax();
    }
    while (hal::read_cycle_counter() < deadline);

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
volatile Flavor kind = FlavorNone;
volatile bool probed;

/* Written by every CPU that arms its counter, always with the same value:
   the profiler asks for one period and hands it to all of them. */
u64 saved_period;

/* Intel: architectural perfmon, fixed counter 1. SDM vol 3B, chapter 20. */
namespace intel
{

const u32 FIXED_CTR1_MSR        = 0x30A;   /* unhalted core cycles */
const u32 FIXED_CTR_CTRL_MSR     = 0x38D;
const u32 GLOBAL_STATUS_MSR     = 0x38E;
const u32 GLOBAL_CTRL_MSR       = 0x38F;
const u32 GLOBAL_OVF_CTRL_MSR    = 0x390;
const u32 DEBUG_CTL_MSR         = 0x1D9;

/* IA32_DEBUGCTL.FREEZE_PERFMON_ON_PMI. Set, the counters stop dead on the
   first overflow and stay stopped; firmware and hypervisors do set it. This
   kernel rearms explicitly, so it wants the bit clear. */
const u64 FREEZE_PERFMON_ON_PMI  = 1ULL << 12;

/* Fixed counter 1 occupies bits 4..7 of the control MSR: OS, USR, AnyThread,
   PMI. Kernel-only counting with an interrupt on overflow. */
const u64 FIXED_CTR1_ENABLE     = (1ULL << 4) | (1ULL << 7);
const u64 FIXED_CTR1_CTRL_MASK   = 0xFULL << 4;

/* Fixed counter i is bit 32+i in the global control and status MSRs. */
const u64 FIXED_CTR1_GLOBAL_BIT  = 1ULL << 33;

u8 counter_width;

u64 counter_mask()
{
    if (counter_width >= 64)
        return ~0ULL;

    return (1ULL << counter_width) - 1;
}

bool detect()
{
    CpuidResult r = cpuid(0xA);

    ulong version = r.eax & 0xFF;
    ulong fixed_count = r.edx & 0x1F;
    ulong fixed_width = (r.edx >> 5) & 0xFF;

    /* Version 2 is where the fixed counters and the global control MSRs
       arrive; counter 1 has to exist, and its width has to be sane. */
    if (version < 2 || fixed_count < 2 || fixed_width == 0 || fixed_width > 64)
        return false;

    counter_width = (u8)fixed_width;
    return true;
}

bool start(ulong period)
{
    u64 mask = counter_mask();
    if ((u64)period >= mask)
        return false;

    write_msr(DEBUG_CTL_MSR, read_msr(DEBUG_CTL_MSR) & ~FREEZE_PERFMON_ON_PMI);

    /* Off while it is being set up. */
    write_msr(GLOBAL_CTRL_MSR, read_msr(GLOBAL_CTRL_MSR) & ~FIXED_CTR1_GLOBAL_BIT);
    write_msr(GLOBAL_OVF_CTRL_MSR, FIXED_CTR1_GLOBAL_BIT);

    u64 ctrl = read_msr(FIXED_CTR_CTRL_MSR) & ~FIXED_CTR1_CTRL_MASK;
    write_msr(FIXED_CTR_CTRL_MSR, ctrl | FIXED_CTR1_ENABLE);

    /* Count up to the wrap, so the overflow lands `period` cycles from now. */
    write_msr(FIXED_CTR1_MSR, (mask - period) & mask);

    /* Deliver the overflow as an NMI: an ordinary vector would be blocked by
       exactly the interrupts-off code this is meant to be able to sample. */
    Lapic::write_lvt_perf_counter_nmi();

    write_msr(GLOBAL_CTRL_MSR, read_msr(GLOBAL_CTRL_MSR) | FIXED_CTR1_GLOBAL_BIT);
    return true;
}

void stop()
{
    write_msr(GLOBAL_CTRL_MSR, read_msr(GLOBAL_CTRL_MSR) & ~FIXED_CTR1_GLOBAL_BIT);
    write_msr(FIXED_CTR_CTRL_MSR, read_msr(FIXED_CTR_CTRL_MSR) & ~FIXED_CTR1_CTRL_MASK);
    write_msr(GLOBAL_OVF_CTRL_MSR, FIXED_CTR1_GLOBAL_BIT);

    Lapic::mask_lvt_perf_counter();
}

bool counts_for_real()
{
    /* Enabled without the PMI bit: a probe this short cannot overflow the
       counter, and an interrupt out of one would arrive with no profiler
       behind it. */
    write_msr(GLOBAL_CTRL_MSR, read_msr(GLOBAL_CTRL_MSR) & ~FIXED_CTR1_GLOBAL_BIT);
    write_msr(FIXED_CTR_CTRL_MSR,
        (read_msr(FIXED_CTR_CTRL_MSR) & ~FIXED_CTR1_CTRL_MASK) | (1ULL << 4));
    write_msr(FIXED_CTR1_MSR, 0);
    write_msr(GLOBAL_CTRL_MSR, read_msr(GLOBAL_CTRL_MSR) | FIXED_CTR1_GLOBAL_BIT);

    bool counts = msr_moves(FIXED_CTR1_MSR, counter_mask());

    stop();
    return counts;
}

bool ack_overflow()
{
    if (!(read_msr(GLOBAL_STATUS_MSR) & FIXED_CTR1_GLOBAL_BIT))
        return false;

    u64 mask = counter_mask();

    /* Rearm before clearing: the counter is already counting again, and the
       order costs nothing. */
    write_msr(FIXED_CTR1_MSR, (mask - saved_period) & mask);
    write_msr(GLOBAL_OVF_CTRL_MSR, FIXED_CTR1_GLOBAL_BIT);

    /* Delivering through the local APIC's performance-counter entry sets its
       mask bit; without clearing it there is exactly one sample per run. */
    Lapic::write_lvt_perf_counter_nmi();

    /* Belt and braces against a machine that froze the counter on the PMI
       anyway: reasserting the enable costs one write and is the difference
       between a profile and a single sample. */
    write_msr(GLOBAL_CTRL_MSR, read_msr(GLOBAL_CTRL_MSR) | FIXED_CTR1_GLOBAL_BIT);
    return true;
}

}

/* AMD: general counter 0, event PMCx076. PPR for family 17h/19h, section
   "Performance Monitor Counters". */
namespace amd
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
const u32 CTL0_MSR = 0xC0010200;
const u32 CTR0_MSR = 0xC0010201;

/* PerfMonV2 (Zen 4 and later): global control and status, Intel-shaped.
   Counter 0 is bit 0 in all three. */
const u32 GLOBAL_STATUS_MSR    = 0xC0010300;
const u32 GLOBAL_CTL_MSR       = 0xC0010301;
const u32 GLOBAL_STATUS_CLR_MSR = 0xC0010302;
const u64 GLOBAL_CTR0_BIT      = 1ULL << 0;

/* PMCx076, "CPU Clocks not Halted": the same thing Intel's fixed counter 1
   counts, and the one AMD event number that has not moved between families
   since the K8. */
const u64 EVENT_CYCLES_NOT_HALTED = 0x76;

/* PERF_CTL: EventSelect[7:0] at 0, UnitMask at 8, USR 16, OS 17, INT 20
   (raise the local APIC's performance-counter interrupt on overflow), EN 22,
   EventSelect[11:8] at 32. Kernel-only, interrupt on overflow -- USR is left
   clear for the same reason Intel's is. */
const u64 CTL_OS   = 1ULL << 17;
const u64 CTL_INT  = 1ULL << 20;
const u64 CTL_EN   = 1ULL << 22;

/* Every AMD performance counter is 48 bits wide, and no CPUID field says so
   -- unlike Intel, where the width is enumerated. */
const ulong COUNTER_WIDTH = 48;
const u64 COUNTER_MASK = (1ULL << COUNTER_WIDTH) - 1;
const u64 COUNTER_SIGN_BIT = 1ULL << (COUNTER_WIDTH - 1);

const u32 PERF_CTR_EXT_CORE_BIT = 1u << 23;   /* CPUID Fn8000_0001_ECX */
const u32 PERF_MON_V2_BIT      = 1u << 0;    /* CPUID Fn8000_0022_EAX */

bool v2;

bool detect()
{
    u32 max_extended = cpuid(0x80000000).eax;
    if (max_extended < 0x80000001)
        return false;

    /* The one gate on the counter MSRs. It also happens to exclude QEMU's
       default `qemu64`, which calls itself an AuthenticAMD family 0Fh part
       with no performance counters behind the claim. */
    if (!(cpuid(0x80000001).ecx & PERF_CTR_EXT_CORE_BIT))
        return false;

    v2 = false;
    if (max_extended >= 0x80000022)
    {
        CpuidResult r22 = cpuid(0x80000022);

        /* Fn8000_0022_EBX[3:0] is the number of core counters. A part that
           claims PerfMonV2 and no counters is one to walk away from rather
           than to write a global enable bit for. */
        if ((r22.eax & PERF_MON_V2_BIT) != 0)
        {
            if ((r22.ebx & 0xF) == 0)
                return false;

            v2 = true;
        }
    }

    return true;
}

bool start(ulong period)
{
    /* The counter is armed below zero, so the period has to fit in the
       counter with its sign bit to spare. */
    if ((u64)period >= COUNTER_SIGN_BIT)
        return false;

    /* Off while it is being set up. Writing the whole control register
       clears the enable along with any event a previous owner left there. */
    write_msr(CTL0_MSR, 0);

    if (v2)
    {
        write_msr(GLOBAL_CTL_MSR, read_msr(GLOBAL_CTL_MSR) & ~GLOBAL_CTR0_BIT);
        write_msr(GLOBAL_STATUS_CLR_MSR, GLOBAL_CTR0_BIT);
    }

    /* Count up to the wrap from `period` cycles below it. On a part without
       the global status register this negative start is also how the
       overflow is recognised later: the sign bit is set now and clear once
       the counter has passed zero. */
    write_msr(CTR0_MSR, (0 - (u64)period) & COUNTER_MASK);

    /* Deliver the overflow as an NMI, for the same reason Intel does: an
       ordinary vector would be blocked by exactly the interrupts-off code
       this is meant to be able to sample. The local APIC entry is the same
       one on both vendors -- only the counter differs. */
    Lapic::write_lvt_perf_counter_nmi();

    write_msr(CTL0_MSR, EVENT_CYCLES_NOT_HALTED | CTL_OS | CTL_INT | CTL_EN);

    /* On a PerfMonV2 part the per-counter enable is not enough on its own:
       the global bit gates it, and it resets to zero. Writing this MSR on a
       part without PerfMonV2 would be a #GP, hence the flag. */
    if (v2)
        write_msr(GLOBAL_CTL_MSR, read_msr(GLOBAL_CTL_MSR) | GLOBAL_CTR0_BIT);

    return true;
}

void stop()
{
    if (v2)
    {
        write_msr(GLOBAL_CTL_MSR, read_msr(GLOBAL_CTL_MSR) & ~GLOBAL_CTR0_BIT);
        write_msr(GLOBAL_STATUS_CLR_MSR, GLOBAL_CTR0_BIT);
    }

    write_msr(CTL0_MSR, 0);

    Lapic::mask_lvt_perf_counter();
}

bool counts_for_real()
{
    /* Counting from zero with the interrupt bit clear: nothing here can
       overflow, and nothing here can raise an NMI with no profiler behind
       it. */
    write_msr(CTL0_MSR, 0);
    if (v2)
    {
        write_msr(GLOBAL_CTL_MSR, read_msr(GLOBAL_CTL_MSR) & ~GLOBAL_CTR0_BIT);
        write_msr(GLOBAL_STATUS_CLR_MSR, GLOBAL_CTR0_BIT);
    }

    write_msr(CTR0_MSR, 0);
    write_msr(CTL0_MSR, EVENT_CYCLES_NOT_HALTED | CTL_OS | CTL_EN);

    if (v2)
        write_msr(GLOBAL_CTL_MSR, read_msr(GLOBAL_CTL_MSR) | GLOBAL_CTR0_BIT);

    bool counts = msr_moves(CTR0_MSR, COUNTER_MASK);

    stop();
    return counts;
}

bool ack_overflow()
{
    if (v2)
    {
        if (!(read_msr(GLOBAL_STATUS_MSR) & GLOBAL_CTR0_BIT))
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
        if (read_msr(CTR0_MSR) & COUNTER_SIGN_BIT)
            return false;
    }

    write_msr(CTR0_MSR, (0 - saved_period) & COUNTER_MASK);

    if (v2)
        write_msr(GLOBAL_STATUS_CLR_MSR, GLOBAL_CTR0_BIT);

    /* Harmless on a part that does not mask its performance-counter entry on
       delivery, and the difference between a profile and one sample on a
       part that does. */
    Lapic::write_lvt_perf_counter_nmi();
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
volatile u8 spurious_credit[MAX_CPUS];

/* Counted rather than silent: a credit spent after the profiler has stopped
   is an NMI this kernel swallowed, and `lscpu` says how many. Per CPU, like
   everything else an NMI writes here, so counting one costs no atomic and
   loses nothing to a simultaneous NMI on another core. */
ulong spurious_nmis[MAX_CPUS];

/* Whether this CPU currently has its counter armed. Load-bearing on AMD
   without PerfMonV2, where an overflow is recognised by the sign bit of a
   counter that keeps whatever value it stopped at: once the profiler has
   run, that bit is as likely clear as not, and without this flag the next
   genuine NMI -- arriving minutes later, from something else entirely --
   would be read as a sample and swallowed instead of panicking. */
volatile u8 armed[MAX_CPUS];

}

bool Pmu::available()
{
    if (probed)
    {
        /* Pairs with the publish below: Kind and everything detect() left
           behind it must be visible to a CPU that has seen Probed. */
        hal::smp_rmb();
        return kind != FlavorNone;
    }

    /* Interrupts off for the whole probe, which is bounded at SELF_TEST_CYCLES
       -- about ten microseconds, once per boot.

       Something has to pin this to one CPU: it runs in task context, and
       select_next_task_queue() will move even a running task to a lighter
       queue, so without pinning the two reads bracketing the spin can come
       off two different CPUs' counters. Task::set_cpu_affinity() would pin it
       too, but it would not stop a tick on this CPU from arming the counter
       underneath the probe -- and for ten microseconds the cheap answer is
       also the complete one. */
    ulong flags = hal::irq_save();

    Flavor found = FlavorNone;

    CpuidResult r0 = cpuid(0);

    static const u32 INTEL_VENDOR_EBX = 0x756E6547;   /* "Genu" */
    static const u32 AMD_VENDOR_EBX   = 0x68747541;   /* "Auth" */

    if (r0.ebx == INTEL_VENDOR_EBX)
    {
        if (r0.eax >= 0xA && intel::detect())
            found = FlavorIntel;
    }
    else if (r0.ebx == AMD_VENDOR_EBX)
    {
        if (amd::detect())
            found = FlavorAmd;
    }

    /* CPUID has said the counter is there. Now make it count: under a
       hypervisor that does not virtualise the PMU the writes are accepted
       and the counter never moves, and a profiler that trusted CPUID would
       arm it on every CPU and report an empty profile with no hint as to
       why. Bounded at SELF_TEST_CYCLES, once. */
    if (found == FlavorIntel && !intel::counts_for_real())
        found = FlavorNone;
    else if (found == FlavorAmd && !amd::counts_for_real())
        found = FlavorNone;

    /* Published after the answer is complete, and after a barrier: the CPU
       running the shell gets here first, and the CPUs that go on to arm their
       own counters read Kind, counter_width and V2 on the strength of having
       seen Probed. */
    kind = found;
    hal::smp_wmb();
    probed = true;

    hal::irq_restore(flags);
    return found != FlavorNone;
}

bool Pmu::start(ulong period)
{
    /* available(), not Probed, is what decides whether there is a counter --
       but this runs from the tick, in an interrupt, and available() spins for
       tens of microseconds the first time it is asked. The profiler calls it
       from the shell before it sets a single CPU sampling, so by the time any
       tick gets here the answer is already in. If it is not, this tick stays
       on the tick. */
    if (!probed)
        return false;

    hal::smp_rmb();
    if (kind == FlavorNone)
        return false;

    /* A CPU with no slot in the armed table would take overflows that
       ack_overflow() then refuses to claim, and every one of them would reach
       the panic. Leave that CPU on the tick instead. */
    ulong index = hal::get_current_cpu_hw_id();
    if (index >= MAX_CPUS)
        return false;

    if (period < MIN_PERIOD)
        period = MIN_PERIOD;

    saved_period = period;

    /* Armed before the counter is, not after: an overflow landing between the
       two is this profiler's, and the flag is what says so. */
    armed[index] = 1;

    if (!((kind == FlavorIntel) ? intel::start(period) : amd::start(period)))
    {
        armed[index] = 0;
        return false;
    }

    return true;
}

void Pmu::stop()
{
    if (kind == FlavorIntel)
        intel::stop();
    else if (kind == FlavorAmd)
        amd::stop();

    /* After the counter is off, not before: an overflow in between is still
       this profiler's, and one that arrives after is a stopped counter's
       leftover value, which is exactly what the flag exists to disregard. */
    ulong index = hal::get_current_cpu_hw_id();
    if (index < MAX_CPUS)
        armed[index] = 0;

    /* The spurious credit is deliberately left standing across the stop. The
       in-flight NMI that it exists for can arrive after the counter is
       disarmed, and swallowing one stray NMI at the end of a profiling run
       is a better trade than panicking on it. */
}

bool Pmu::ack_overflow()
{
    /* saved_period is 0 until some CPU has armed the counter. Rearming with it
       would set the counter one cycle short of its wrap, and the machine
       would never leave the NMI. */
    if (kind == FlavorNone || saved_period == 0)
        return false;

    ulong index = hal::get_current_cpu_hw_id();
    if (index >= MAX_CPUS || !armed[index])
        return false;

    if (!((kind == FlavorIntel) ? intel::ack_overflow() : amd::ack_overflow()))
        return false;

    spurious_credit[index] = 1;
    return true;
}

bool Pmu::absorb_spurious_nmi()
{
    if (kind != FlavorAmd)
        return false;

    ulong index = hal::get_current_cpu_hw_id();
    if (index >= MAX_CPUS || spurious_credit[index] == 0)
        return false;

    spurious_credit[index] = 0;

    /* Counted, not traced: Trace takes the dmesg lock, and this runs in an
       NMI that may have landed on a CPU already holding it. `lscpu` reads
       the count where printing it is safe. */
    spurious_nmis[index]++;
    return true;
}

ulong Pmu::spurious_nmi_count()
{
    ulong total = 0;
    for (ulong i = 0; i < MAX_CPUS; i++)
        total += spurious_nmis[i];

    return total;
}

const char* Pmu::name()
{
    switch (kind)
    {
    case FlavorIntel:
        return "pmu/nmi intel fixed-ctr1";
    case FlavorAmd:
        return amd::v2 ? "pmu/nmi amd pmc0 v2" : "pmu/nmi amd pmc0";
    default:
        return "pmu/nmi";
    }
}

}
