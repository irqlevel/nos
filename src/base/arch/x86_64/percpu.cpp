#include "percpu.h"
#include "asm.h"

#include <kernel/cpu.h>
#include <kernel/panic.h>

namespace kernel
{

namespace
{

const u32 GS_BASE_MSR = 0xC0000101; /* IA32_GS_BASE */

/* Zero-initialised, which is exactly the "not set up" encoding. Shared by
   every CPU that has not published yet; nothing is ever written to it. */
PerCpuData boot_slot;

PerCpuData slot[MAX_CPUS];

}

void per_cpu_setup_boot()
{
    write_msr(GS_BASE_MSR, (ulong)&boot_slot);
}

void per_cpu_publish(ulong apic_id)
{
    /* An APIC ID this kernel cannot index is already fatal elsewhere (it is
       the CPU index); here it just means this CPU keeps paying for the MMIO
       read rather than scribbling outside the array. */
    if (BUG_ON(apic_id >= MAX_CPUS))
        return;

    slot[apic_id].apic_id_plus_one = apic_id + 1;
    write_msr(GS_BASE_MSR, (ulong)&slot[apic_id]);
}

}
