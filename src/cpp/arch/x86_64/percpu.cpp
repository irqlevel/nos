#include "percpu.h"
#include "asm.h"

#include <kernel/cpu.h>
#include <kernel/panic.h>

namespace Kernel
{

namespace
{

const u32 GsBaseMsr = 0xC0000101; /* IA32_GS_BASE */

/* Zero-initialised, which is exactly the "not set up" encoding. Shared by
   every CPU that has not published yet; nothing is ever written to it. */
PerCpuData BootSlot;

PerCpuData Slot[MaxCpus];

}

void PerCpuSetupBoot()
{
    WriteMsr(GsBaseMsr, (ulong)&BootSlot);
}

void PerCpuPublish(ulong apicId)
{
    /* An APIC ID this kernel cannot index is already fatal elsewhere (it is
       the CPU index); here it just means this CPU keeps paying for the MMIO
       read rather than scribbling outside the array. */
    if (BugOn(apicId >= MaxCpus))
        return;

    Slot[apicId].ApicIdPlusOne = apicId + 1;
    WriteMsr(GsBaseMsr, (ulong)&Slot[apicId]);
}

}
