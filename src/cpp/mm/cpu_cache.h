#pragma once

#include <hal/irqchip.h>
#include <include/types.h>

namespace Kernel
{

namespace Mm
{

/* The CPUs the heap keeps a cache for -- AllocatorImpl's in front of the
   pools, PageAllocatorImpl's in front of its kept blocks: Kernel::MaxCpus
   (kernel/cpu.h), which mm's headers cannot include; allocator.cpp checks
   that the two agree. */
static constexpr size_t CacheCpus = 64;

/* This CPU's index into the heap's per-CPU caches; false while it cannot
   yet say which CPU it is -- on x86 a CPU learns that from its local APIC
   until it has published its per-CPU slot, and there is no APIC to ask
   before the ACPI tables are read, which the heap's first allocations
   precede -- or for an index past the caches. A task moved to another CPU
   after this reads the wrong cache, which that cache's lock keeps as safe
   as the right one. */
static inline bool CacheCpu(ulong& cpu)
{
    if (!Hal::IrqChipReady())
        return false;

    cpu = Hal::GetCurrentCpuHwId();
    return cpu < CacheCpus;
}

}
}
