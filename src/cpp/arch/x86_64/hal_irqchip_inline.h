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
    return Kernel::Lapic::GetApicId();
}

static inline __attribute__((always_inline)) void SendIpi(ulong hwId, u8 vector)
{
    Kernel::Lapic::SendIPI(hwId, vector);
}

}
