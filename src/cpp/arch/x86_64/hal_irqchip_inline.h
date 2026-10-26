#pragma once

#include <include/types.h>
#include <arch/x86_64/lapic.h>
#include <drivers/acpi.h>

// x86_64 bodies for the Hal:: irqchip wrappers (see hal/irqchip.h).
// "hwId" is the LAPIC id, which this kernel keeps equal to the cpu index.

namespace hal
{

/* IPI vector: IDT slot on x86, SGI INTID on arm64 */
constexpr u8 IPI_VECTOR = 0xFE;

/* The vector a vCPU running a guest is sent to hand it an interrupt posted
   for it (VT-x's posted interrupts, hvarch): taken by the CPU in its guest
   without an exit, and in the host -- the guest just left -- an EOI and
   nothing more (PostedInterrupt). Above every device's and below the
   timer's. */
constexpr u8 POSTED_VECTOR = 0xFC;

/* True once the LAPIC MMIO is discovered: cpu-id reads and IPIs are safe */
static inline __attribute__((always_inline)) bool irq_chip_ready()
{
    return kernel::Acpi::get_instance().get_lapic_address() != nullptr;
}

static inline __attribute__((always_inline)) void irq_eoi()
{
    kernel::Lapic::eoi();
}

static inline __attribute__((always_inline)) void irq_eoi(u8 vector)
{
    kernel::Lapic::eoi(vector);
}

static inline __attribute__((always_inline)) bool irq_is_in_service(u8 vector)
{
    return kernel::Lapic::check_isr(vector);
}

static inline __attribute__((always_inline)) ulong get_current_cpu_hw_id()
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

    return kernel::Lapic::get_apic_id();
}

static inline __attribute__((always_inline)) void send_ipi(ulong hw_id, u8 vector)
{
    kernel::Lapic::send_ipi(hw_id, vector);
}

/* An NMI reaches a CPU that has interrupts disabled -- which is exactly the
   CPU worth asking about when the machine has wedged, and exactly the one an
   ordinary IPI cannot reach. */
static inline __attribute__((always_inline)) bool nmi_ipi_supported()
{
    return true;
}

static inline __attribute__((always_inline)) void send_nmi_ipi(ulong hw_id)
{
    kernel::Lapic::send_nmi(hw_id);
}

}
