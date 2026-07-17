#pragma once

#include <include/types.h>
#include <arch/arm64/gicv3.h>

// arm64 bodies for the Hal:: irqchip wrappers (see hal/irqchip.h).
// "hwId" is the kernel's linear CPU index (cached in TPIDR_EL1); SendSgi
// resolves it to the MPIDR affinity via the Board CPU list.

namespace Hal
{

/* IPI vector: IDT slot on x86, SGI INTID on arm64 */
constexpr u8 IpiVector = 1;

static inline __attribute__((always_inline)) ulong GetCurrentCpuHwId()
{
    /* Per-CPU index cached in TPIDR_EL1 at CPU startup (boot.S), so this is
       a single sysreg read and does not depend on the MPIDR affinity
       encoding (Aff0 is not necessarily the linear index on real hw). */
    ulong idx;
    asm volatile("mrs %0, tpidr_el1" : "=r"(idx));
    return idx;
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
