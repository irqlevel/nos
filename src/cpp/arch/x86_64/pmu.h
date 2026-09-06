#pragma once

#include <include/types.h>

namespace Kernel
{

/* Performance monitoring, used for one thing: overflowing a cycle counter
   into an NMI so the profiler can sample far faster than the tick, and can
   sample code that is running with interrupts disabled -- which the tick, by
   definition, never sees.

   Two entirely different interfaces count the same cycles here.

   Intel: architectural perfmon version 2 or later, fixed counter 1 --
   unhalted core cycles on every part that has architectural perfmon at all,
   including the hybrid ones where the two core types do not agree about
   anything else. Overflow is reported exactly, by a global status MSR.

   AMD: there are no fixed counters, so general counter 0 is programmed with
   event PMCx076, "CPU Clocks not Halted" -- the same quantity, under the one
   event number that has not moved across AMD families. The MSRs are a
   different set (legacy at 0xC0010000, the six-counter core extension at
   0xC0010200), and on parts before Zen 4 there is no global status register
   at all: the counter is armed below zero and the overflow is recognised by
   its sign bit going clear. Zen 4 and later add PerfMonV2, whose global
   control and status work like Intel's and are used when present.

   Available() answers false everywhere else, and the profiler keeps its
   tick. */
class Pmu final
{
public:
    /* CPUID says which of the two, if either, this part speaks -- and then
       the counter is asked to prove it counts. A hypervisor can answer CPUID
       for a performance monitor it does not virtualise and swallow every MSR
       write to it, and nothing in CPUID tells that machine apart from one
       where the counters are real. Both answers are cached: this is asked on
       the CPU that starts a profiling run, and the CPUs that then arm their
       own counters must not each repeat the probe. */
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

    /* Called from the NMI handler once AckOverflow() has said no. True when
       this NMI is an AMD part's habit of delivering a performance-counter
       interrupt just after the overflow that caused it was handled: not a
       sample, but not a fault to panic over either. */
    static bool AbsorbSpuriousNmi();

    /* Which counter the samples come off, for the profile report and for
       `lscpu`. */
    static const char* Name();

    /* How many late NMIs AbsorbSpuriousNmi() has swallowed. Counted rather
       than traced: the count is read from the shell, where printing it is
       safe, and an NMI handler is no place to take the dmesg lock. */
    static ulong SpuriousNmiCount();

    /* Cycles between samples: about 1 kHz on a 3 GHz part, ten times the
       tick and enough to see inside a function without drowning the machine
       in interrupts. */
    static const ulong DefaultPeriod = 3000000;
    static const ulong MinPeriod = 100000;
};

}
