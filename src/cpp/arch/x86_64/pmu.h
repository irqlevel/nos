#pragma once

#include <include/types.h>

namespace Kernel
{

/* Architectural performance monitoring, used for one thing: overflowing a
   cycle counter into an NMI so the profiler can sample far faster than the
   tick, and can sample code that is running with interrupts disabled --
   which the tick, by definition, never sees.
 
   Intel only. AMD counts the same thing through entirely different MSRs, and
   writing that blind, with no AMD machine to run it on, would be guessing;
   Available() simply says no there and the profiler keeps its tick.
 
   The fixed counters are used rather than a programmable event: counter 1 is
   unhalted core cycles on every part that has architectural perfmon at all,
   including the hybrid ones where the two core types do not agree about
   anything else. */
class Pmu final
{
public:
    /* CPUID.0AH says whether any of this exists. */
    static bool Available();

    /* Arm this CPU's counter to raise an NMI every `period` core cycles.
       The period is clamped from below: a period small enough to overflow
       faster than the handler returns is a machine that never leaves the
       NMI. */
    static bool Start(ulong period);

    /* Disarm this CPU's counter. */
    static void Stop();

    /* Called from the NMI handler. True when the interrupt was a counter
       overflow -- in which case the counter has been rearmed and the local
       APIC's perf entry unmasked, and the caller should treat it as a
       sample rather than as a fault. */
    static bool AckOverflow();

    /* Cycles between samples: about 1 kHz on a 3 GHz part, ten times the
       tick and enough to see inside a function without drowning the machine
       in interrupts. */
    static const ulong DefaultPeriod = 3000000;
    static const ulong MinPeriod = 100000;

private:
    static u64 CounterMask();
};

}
