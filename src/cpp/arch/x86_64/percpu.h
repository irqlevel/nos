#pragma once

#include <include/types.h>

namespace Kernel
{

/* Per-CPU data reachable through the GS base, so asking "which CPU am I on"
   costs one segment-prefixed load instead of an uncached MMIO read of the
   local APIC's ID register.
 
   That question is asked on the packet datapath (NetFramePool, once when a
   frame is taken and once when it comes back), from the profiler's NMI, and
   from every CpuTable::GetCurrentCpu -- which the watchdog, the scheduler and
   the timer wheel each reach on every tick. arm64 has always answered it from
   TPIDR_EL1; this is the x86 twin. x2APIC, whose ID lives in an MSR, is not
   an option here: this kernel deliberately switches the APIC back to xAPIC
   MMIO mode when firmware hands it over in x2APIC mode.
 
   There is no user mode in this kernel, so GS never has to be swapped: it is
   set once per CPU and left alone.
 
   The slot holds the APIC ID biased by one, so that the all-zero BSS a CPU
   sees before it has published anything means "not set up yet" rather than
   "APIC ID 0", which is a real CPU. */
struct PerCpuData
{
    ulong ApicIdPlusOne;
};

/* Point GS at a shared slot that reads as "not set up". Must run before
   anything on this CPU can ask for its ID, because a GS base left as
   whatever firmware had would make that question a wild read. */
void PerCpuSetupBoot();

/* Give this CPU its own slot, once its APIC ID is known. */
void PerCpuPublish(ulong apicId);

}
