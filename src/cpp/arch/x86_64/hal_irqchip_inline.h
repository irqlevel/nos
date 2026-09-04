#pragma once

#include <include/types.h>
#include <arch/x86_64/lapic.h>
#include <drivers/acpi.h>

// x86_64 bodies for the Hal:: irqchip wrappers (see hal/irqchip.h).
// "hwId" is the LAPIC id, which this kernel keeps equal to the cpu index.

namespace Hal
{

/* IPI vector: IDT slot on x86, SGI INTID on arm64 */
constexpr u8 IpiVector = 0xFE;

/* True once the LAPIC MMIO is discovered: cpu-id reads and IPIs are safe */
static inline __attribute__((always_inline)) bool IrqChipReady()
{
    return Kernel::Acpi::GetInstance().GetLapicAddress() != nullptr;
}

static inline __attribute__((always_inline)) void IrqEoi()
{
    Kernel::Lapic::EOI();
}

static inline __attribute__((always_inline)) void IrqEoi(u8 vector)
{
    Kernel::Lapic::EOI(vector);
}

static inline __attribute__((always_inline)) bool IrqIsInService(u8 vector)
{
    return Kernel::Lapic::CheckIsr(vector);
}

static inline __attribute__((always_inline)) ulong GetCurrentCpuHwId()
{
    /* One load through GS instead of an uncached MMIO read of the APIC's ID
       register -- see arch/x86_64/percpu.h for why this is on a hot enough
       path to be worth a segment register.

       Zero means this CPU has not published its slot yet, which is the window
       between entering C++ and Lapic::Enable(); the MMIO read is still
       correct there, and is what the whole kernel used to do. */
    ulong biased;
    asm volatile("movq %%gs:0x0, %0" : "=r"(biased));

    if (biased != 0)
        return biased - 1;

    return Kernel::Lapic::GetApicId();
}

static inline __attribute__((always_inline)) void SendIpi(ulong hwId, u8 vector)
{
    Kernel::Lapic::SendIPI(hwId, vector);
}

/* An NMI reaches a CPU that has interrupts disabled -- which is exactly the
   CPU worth asking about when the machine has wedged, and exactly the one an
   ordinary IPI cannot reach. */
static inline __attribute__((always_inline)) bool NmiIpiSupported()
{
    return true;
}

static inline __attribute__((always_inline)) void SendNmiIpi(ulong hwId)
{
    Kernel::Lapic::SendNmi(hwId);
}

}
