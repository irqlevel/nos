#pragma once

#include <include/types.h>
#include <arch/arm64/gicv3.h>

// arm64 bodies for the Hal:: irqchip wrappers (see hal/irqchip.h).
// "hwId" is MPIDR_EL1.Aff0, which QEMU virt numbers linearly 0..N-1.

namespace Hal
{

/* IPI vector: IDT slot on x86, SGI INTID on arm64 */
constexpr u8 IpiVector = 1;

static inline __attribute__((always_inline)) ulong GetCurrentCpuHwId()
{
    ulong mpidr;
    asm volatile("mrs %0, mpidr_el1" : "=r"(mpidr));
    return mpidr & 0xFF;
}

static inline __attribute__((always_inline)) bool IrqChipReady()
{
    return true; /* cpu-id reads (MPIDR) never need the GIC */
}

/* No-arg EOI is x86-only (LAPIC); on arm64 the IRQ dispatch loop owns
   the IAR/EOIR pairing (see arch/arm64/interrupt_arm64.cpp). */
static inline __attribute__((always_inline)) void IrqEoi()
{
}

static inline __attribute__((always_inline)) void IrqEoi(u8 vector)
{
    Kernel::Gic::WriteEoir(vector);
}

static inline __attribute__((always_inline)) bool IrqIsInService(u8 vector)
{
    (void)vector;
    return false;
}

static inline __attribute__((always_inline)) void SendIpi(ulong hwId, u8 vector)
{
    Kernel::Gic::GetInstance().SendSgi(hwId, vector);
}

}
