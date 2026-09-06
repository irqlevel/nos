#pragma once

#include <include/types.h>

/* Performance-counter sampling, for the profiler: overflow a cycle counter
   into an NMI so samples arrive faster than the tick, and arrive even from
   code running with interrupts disabled -- which the tick never sees.

   Every one of these is per CPU: the counter and its control live in that
   CPU's own model-specific registers, so each core arms and disarms its own.

   x86-64 implements it on Intel parts with architectural perfmon version 2
   or later, and on AMD family 10h and later; everywhere else PmuAvailable()
   answers false and the profiler keeps sampling on the tick. */
namespace Hal
{

bool PmuAvailable();
bool PmuStart();
void PmuStop();

/* What the samples are being counted with, for the profile report: the two
   vendors reach the same cycle count through different counters, and a
   report that says which one is what tells a PMU the hypervisor declined to
   virtualise apart from a machine that was simply idle. */
const char* PmuName();

}
