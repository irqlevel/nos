#pragma once

#include <include/types.h>
#include <arch/x86_64/lapic.h>

// x86_64 bodies for the Hal:: irqchip wrappers (see hal/irqchip.h).
// "hwId" is the LAPIC id, which this kernel keeps equal to the cpu index.

namespace Hal
{

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
