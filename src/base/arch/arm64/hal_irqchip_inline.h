#pragma once

#include <include/types.h>
#include <arch/arm64/gicv3.h>

// arm64 bodies for the hal:: irqchip wrappers (see hal/irqchip.h).
// "hw_id" is the kernel's linear CPU index (cached in TPIDR_EL1); send_sgi
// resolves it to the MPIDR affinity via the Board CPU list.

namespace hal
{

/* IPI vector: IDT slot on x86, SGI INTID on arm64 */
constexpr u8 IPI_VECTOR = 1;

/* No posted interrupts here: a vCPU's notification is the IPI. */
constexpr u8 POSTED_VECTOR = IPI_VECTOR;

static inline __attribute__((always_inline)) ulong get_current_cpu_hw_id()
{
    /* Per-CPU index cached in TPIDR_EL1 at CPU startup (boot.S), so this is
       a single sysreg read and does not depend on the MPIDR affinity
       encoding (Aff0 is not necessarily the linear index on real hw). */
    ulong idx;
    asm volatile("mrs %0, tpidr_el1" : "=r"(idx));
    return idx;
}

static inline __attribute__((always_inline)) bool irq_chip_ready()
{
    return true; /* cpu-id reads (MPIDR) never need the GIC */
}

/* No-arg EOI is x86-only (LAPIC); on arm64 the IRQ dispatch loop owns
   the IAR/EOIR pairing (see arch/arm64/interrupt_arm64.cpp). */
static inline __attribute__((always_inline)) void irq_eoi()
{
}

static inline __attribute__((always_inline)) void irq_eoi(u8 vector)
{
    kernel::Gic::write_eoir(vector);
}

static inline __attribute__((always_inline)) bool irq_is_in_service(u8 vector)
{
    (void)vector;
    return false;
}

static inline __attribute__((always_inline)) void send_ipi(ulong hw_id, u8 vector)
{
    kernel::Gic::get_instance().send_sgi(hw_id, vector);
}

/* ARMv8.0 has no NMI, and GICv3 pseudo-NMI (interrupt priorities plus
   FEAT_IRQ priority masking) is not wired up here yet: the panic path asks
   first and skips remote stack capture when the answer is no. */
static inline __attribute__((always_inline)) bool nmi_ipi_supported()
{
    return false;
}

static inline __attribute__((always_inline)) void send_nmi_ipi(ulong hw_id)
{
    (void)hw_id;
}

}
