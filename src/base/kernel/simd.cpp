#include "simd.h"
#include "atomic.h"
#include "cpu.h"
#include "klog.h"
#include "preempt.h"
#include "panic.h"
#include <hal/simd.h>
#include <hal/barrier.h>

namespace kernel
{
namespace simd
{

namespace
{

/* What a section may use on every CPU that has come up -- each CPU's answer
   folded in as it comes up (setup_cpu) -- FEATURES_UNSET before the first. */
const long FEATURES_UNSET = -1;
Atomic common_features(FEATURES_UNSET);

/* A CPU's section: whether one is open, the task whose preemption it took
   and what it was opened to use. This CPU's alone: written and read on it,
   with preemption off, by the code that opens and closes one there -- an
   interrupt handler's among it, whose section, opened while none was,
   closes before the code it cut into goes on. */
Atomic section_open[MAX_CPUS];
Task* section_task[MAX_CPUS];
u32 section_features[MAX_CPUS];

/* Whose registers each CPU holds: a guest's number (hvarch's GuestFp), or
   0 -- nobody's, as after a section, whose values are nobody's to put
   back. */
Atomic guest_owner[MAX_CPUS];

}

void setup_cpu(bool boot_cpu)
{
    long local = hal::simd_setup_cpu();
    /* Folded in: the CPUs of every machine this runs on are of one kind,
       but a section on any of them may use only what all of them have. A
       CPU coming up with less takes it away from the others' next ones. */
    for (;;)
    {
        long seen = common_features.get();
        long folded = (seen == FEATURES_UNSET) ? local : (seen & local);
        if (common_features.cmpxchg(folded, seen) == seen)
            break;
    }
    if (boot_cpu)
        klog(0, "simd: sections use%s%s%s%s%s%s", (local & hal::SIMD_SSE2) ? " SSE2" : "",
            (local & hal::SIMD_SSSE3) ? " SSSE3" : "", (local & hal::SIMD_AVX) ? " AVX" : "",
            (local & hal::SIMD_AVX2) ? " AVX2" : "", (local & hal::SIMD_NEON) ? " NEON" : "",
            local == 0 ? " nothing: no vector unit" : "");
}

u32 features()
{
    long seen = common_features.get();
    return (seen == FEATURES_UNSET) ? 0 : (u32)seen;
}

bool begin()
{
    u32 usable = features();
    if (usable == 0)
        return false;

    /* Preemption off before the CPU is read: from here the task stays on
       it, and no other task's code runs there until end(). */
    Task* task = preempt_disable_task();
    ulong cpu = CpuTable::get_instance().get_current_cpu_id();
    if (cpu >= MAX_CPUS || section_open[cpu].get() != 0)
    {
        preempt_enable_task(task);
        return false;
    }
    section_open[cpu].set(1);
    section_task[cpu] = task;
    section_features[cpu] = usable;
    hal::compiler_barrier();

    /* A guest's registers on this CPU are its own still -- its exit saved
       them -- but the CPU's are about to be this section's: its next entry
       here has to put them back. */
    guest_owner[cpu].set(0);
    hal::simd_enter(usable);
    return true;
}

void end()
{
    ulong cpu = CpuTable::get_instance().get_current_cpu_id();
    BUG_ON(cpu >= MAX_CPUS || section_open[cpu].get() == 0);
    hal::simd_leave(section_features[cpu]);
    Task* task = section_task[cpu];
    hal::compiler_barrier();
    section_open[cpu].set(0);
    preempt_enable_task(task);
}

bool guest_registers_are(ulong cpu, u64 id)
{
    return cpu < MAX_CPUS && id != 0 && (u64)guest_owner[cpu].get() == id;
}

void guest_registers_loaded(ulong cpu, u64 id)
{
    if (cpu >= MAX_CPUS || id == 0)
        return;
    /* A guest runs on one CPU at a time, so no load of its own races this;
       another guest's on one of those CPUs, or a section there, may put
       its own mark there meanwhile, which the compare-and-swap leaves. */
    for (ulong other = 0; other < MAX_CPUS; other++)
    {
        if (other != cpu && (u64)guest_owner[other].get() == id)
            guest_owner[other].cmpxchg(0, (long)id);
    }
    guest_owner[cpu].set((long)id);
}

void guest_registers_lost(ulong cpu)
{
    if (cpu < MAX_CPUS)
        guest_owner[cpu].set(0);
}

}
}
