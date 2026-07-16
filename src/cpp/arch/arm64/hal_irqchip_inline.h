#pragma once

#include <include/types.h>

// arm64 bodies for the Hal:: irqchip wrappers (see hal/irqchip.h).
// "hwId" is MPIDR_EL1.Aff0, which QEMU virt numbers linearly 0..N-1.
// EOI/IPI/in-service arrive with the GICv3 driver (milestone M3/M4);
// until then they are inert, which is correct for a 1-CPU no-IRQ kernel.

namespace Hal
{

static inline __attribute__((always_inline)) ulong GetCurrentCpuHwId()
{
    ulong mpidr;
    asm volatile("mrs %0, mpidr_el1" : "=r"(mpidr));
    return mpidr & 0xFF;
}

/* Cpu-id reads (MPIDR) are always safe; IPIs become real with the GIC */
static inline __attribute__((always_inline)) bool IrqChipReady()
{
    return true;
}

static inline __attribute__((always_inline)) void IrqEoi()
{
}

static inline __attribute__((always_inline)) void IrqEoi(u8 vector)
{
    (void)vector;
}

static inline __attribute__((always_inline)) bool IrqIsInService(u8 vector)
{
    (void)vector;
    return false;
}

static inline __attribute__((always_inline)) void SendIpi(ulong hwId, u8 vector)
{
    (void)hwId;
    (void)vector;
}

}
