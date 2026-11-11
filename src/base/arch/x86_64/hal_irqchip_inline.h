#pragma once

#include <include/types.h>
#include <arch/x86_64/lapic.h>
#include <drivers/acpi.h>

// x86_64 bodies for the hal:: irqchip wrappers (see hal/irqchip.h).
// "hw_id" is the LAPIC id, which this kernel keeps equal to the cpu index.

namespace hal
{

/* IPI vector: IDT slot on x86, SGI INTID on arm64 */
constexpr u8 IPI_VECTOR = 0xFE;

/* The vector a vCPU running a guest is sent to hand it an interrupt posted
   for it (VT-x's posted interrupts, hvarch): taken by the CPU in its guest
   without an exit, and in the host -- the guest just left -- an EOI and
   nothing more (posted_interrupt). Above every device's and below the
   timer's. */
constexpr u8 POSTED_VECTOR = 0xFC;

/* True once the LAPIC MMIO is discovered: cpu-id reads and IPIs are safe */
static inline __attribute__((always_inline)) bool irq_chip_ready()
{
    return kernel::Acpi::get_instance().get_lapic_address() != nullptr;
}

/* A level line with no device on it, which a self-test may register a
   handler on and take it off again: false where none can be promised
   quiet. Under QEMU and under nos's own hypervisor -- told by the firmware's
   OEM id -- the last pin of an IO-APIC of 24: their IO-APICs raise a pin
   when a device of theirs does and at no other time. On any other machine
   none: a real IO-APIC's pin that nothing here drives may be held down by a
   device nothing here drives, and one left unconnected and programmed
   active-low, as a PCI line is, is asserted for as long as it is
   unmasked. */
static inline bool spare_level_line(u8& line)
{
    static const u8 SPARE_PIN = 23;

    const char* oem = kernel::Acpi::get_instance().get_oem_id();
    if (stdlib::str_cmp(oem, "BOCHS ") != 0 && stdlib::str_cmp(oem, "NOS   ") != 0)
        return false;

    line = SPARE_PIN;
    return true;
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
       between entering C++ and Lapic::enable(); the MMIO read is still
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
