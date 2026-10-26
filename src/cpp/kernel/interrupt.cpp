#include "interrupt.h"
#include <arch/x86_64/idt.h>
#include "cpu.h"
#include "irq_balance.h"
#include "trace.h"
#include <hal/cpu.h>
#include <hal/context.h>
#include <hal/irq_stubs.h>

#include <drivers/acpi.h>
#include <arch/x86_64/ioapic.h>
#include <hal/irqchip.h>

namespace kernel
{

Interrupt::VectorEntry Interrupt::vectors_[MAX_VECTORS];

void Interrupt::register_handler(InterruptHandler& handler, u8 irq, u8 vector)
{
    trace(0, "Register interrupt irq 0x%lX vector 0x%lX fn 0x%p",
        (ulong)irq, (ulong)vector, handler.get_handler_fn());

    /* `irq` is a GSI (callers pass acpi.get_gsi_by_irq()). Claiming a GSI or
       vector that is already owned would silently reprogram the IOAPIC pin
       or overwrite the IDT entry, and the loser's interrupts would be
       EOI'd unhandled by the winner's stub -- refuse loudly instead. */
    for (ulong v = 0; v < MAX_VECTORS; v++)
    {
        VectorEntry& other = vectors_[v];
        if (other.handler_count > 0 && other.gsi == irq)
        {
            trace(0, "Register interrupt: gsi 0x%lX already owned by vector 0x%lX, refused",
                (ulong)irq, v);
            return;
        }
    }
    if (vectors_[vector].handler_count > 0)
    {
        trace(0, "Register interrupt: vector 0x%lX already in use, refused",
            (ulong)vector);
        return;
    }

    IoApic::get_instance().set_irq(irq, CpuTable::get_instance().get_current_cpu_id(), vector);
    Idt::get_instance().set_descriptor(vector, IdtDescriptor::encode(handler.get_handler_fn()));

    /* Record the claim so register_level cannot land a PCI device on the
       same GSI/vector behind this handler's back */
    VectorEntry& ve = vectors_[vector];
    ve.gsi = irq;
    ve.level = false;
    ve.handlers[0] = &handler;
    ve.handler_count = 1;

    handler.on_interrupt_register(irq, vector);
}

void Interrupt::register_level(InterruptHandler& handler, u8 irq, u8 vector)
{
    auto& acpi = Acpi::get_instance();
    u8 gsi = (u8)acpi.get_gsi_by_irq(irq);

    /* Decode ACPI MADT polarity flags for this IRQ.
       Bits [1:0]: 00=bus default, 01=active high, 11=active low.
       PCI bus default is active low. */
    u16 acpi_flags = acpi.get_irq_flags(irq);
    u8 polarity = acpi_flags & 3;
    bool active_high = (polarity == 1);

    /* Check if this GSI is already registered on another vector */
    for (ulong v = 0; v < MAX_VECTORS; v++)
    {
        VectorEntry& ve = vectors_[v];
        if (ve.handler_count > 0 && ve.gsi == gsi)
        {
            if (!ve.level)
            {
                /* An edge-triggered system IRQ (PIT/HPET/8042/serial) owns
                   this GSI. Chaining a level PCI handler onto it would
                   re-program the pin and feed the edge handler phantom
                   interrupts -- refuse loudly instead. */
                trace(0, "Register level interrupt irq 0x%lX: gsi 0x%lX owned by edge vector 0x%lX, refused",
                    (ulong)irq, (ulong)gsi, v);
                return;
            }

            /* GSI already registered -- chain this handler onto the existing vector */
            if (ve.handler_count >= MAX_SHARED_HANDLERS)
            {
                trace(0, "Register level interrupt irq 0x%lX: too many shared handlers", (ulong)irq);
                return;
            }

            ve.handlers[ve.handler_count] = &handler;
            ve.handler_count++;

            /* Switch to shared dispatch stub */
            Idt::get_instance().set_descriptor((u8)v, IdtDescriptor::encode(shared_interrupt_stub));

            trace(0, "Register shared level interrupt irq 0x%lX vector 0x%lX handler %lu",
                (ulong)irq, (ulong)v, (ulong)ve.handler_count);

            handler.on_interrupt_register(irq, (u8)v);
            return;
        }
    }

    /* Fresh GSI: the requested vector must be free, or we would silently
       overwrite another device's IDT entry */
    if (vectors_[vector].handler_count > 0)
    {
        trace(0, "Register level interrupt irq 0x%lX: vector 0x%lX already in use, refused",
            (ulong)irq, (ulong)vector);
        return;
    }

    trace(0, "Register level interrupt irq 0x%lX gsi 0x%lX vector 0x%lX fn 0x%p active_high %lu",
        (ulong)irq, (ulong)gsi, (ulong)vector, handler.get_handler_fn(), (ulong)active_high);

    ulong cpu = IrqBalance::get_instance().assign_io_apic_irq(gsi);
    IoApic::get_instance().set_irq_level(gsi, cpu, vector, active_high);

    /* Record in the shared table */
    VectorEntry& ve = vectors_[vector];
    ve.gsi = gsi;
    ve.level = true;
    ve.handlers[0] = &handler;
    ve.handler_count = 1;

    /* First handler -- use device's own stub */
    Idt::get_instance().set_descriptor(vector, IdtDescriptor::encode(handler.get_handler_fn()));

    handler.on_interrupt_register(irq, vector);
}

void Interrupt::shared_dispatch(Context* ctx)
{
    InterruptStats::inc(IrqShared);

    for (ulong v = 0; v < MAX_VECTORS; v++)
    {
        VectorEntry& ve = vectors_[v];
        /* Only dispatch the vector that is actually in service; handlers
           of unrelated shared vectors must not see phantom interrupts */
        if (ve.handler_count > 1 && hal::irq_is_in_service((u8)v))
        {
            for (u8 i = 0; i < ve.handler_count; i++)
            {
                InterruptHandler* h = ve.handlers[i];
                if (h)
                    h->on_interrupt(ctx);
            }
        }
    }

    hal::irq_eoi();
}

}

extern "C" void shared_interrupt(kernel::Context* ctx)
{
    kernel::Interrupt::shared_dispatch(ctx);
}