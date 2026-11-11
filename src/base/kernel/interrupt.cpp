#include "interrupt.h"
#include <arch/x86_64/idt.h>
#include "cpu.h"
#include "irq_balance.h"
#include "klog.h"
#include "tracepoint.h"
#include <hal/cpu.h>
#include <hal/context.h>
#include <hal/irq_stubs.h>

#include <drivers/acpi.h>
#include <arch/x86_64/ioapic.h>
#include <hal/irqchip.h>

namespace kernel
{

Interrupt::VectorEntry Interrupt::vectors_[MAX_VECTORS];

bool Interrupt::is_vector_free(u8 vector)
{
    return __atomic_load_n(&vectors_[vector].handler_count, __ATOMIC_RELAXED) == 0;
}

bool Interrupt::register_handler(InterruptHandler& handler, u8 irq, u8 vector)
{
    klog(0, "Register interrupt irq 0x%lX vector 0x%lX fn 0x%p",
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
            klog(0, "Register interrupt: gsi 0x%lX already owned by vector 0x%lX, refused",
                (ulong)irq, v);
            return false;
        }
    }
    if (vectors_[vector].handler_count > 0)
    {
        klog(0, "Register interrupt: vector 0x%lX already in use, refused",
            (ulong)vector);
        return false;
    }

    /* A GSI past the IO-APIC's pins is another IO-APIC's, or nobody's: the
       write for it used to go out all the same, the register's index a
       byte that wrapped -- onto the IO-APIC's id, or another pin's entry */
    if (!IoApic::get_instance().set_irq(irq, CpuTable::get_instance().get_current_cpu_id(), vector))
    {
        klog(0, "Register interrupt: gsi 0x%lX is no pin of the IO-APIC's, refused", (ulong)irq);
        return false;
    }
    Idt::get_instance().set_descriptor(vector, IdtDescriptor::encode(handler.get_handler_fn()));

    /* Record the claim so register_level cannot land a PCI device on the
       same GSI/vector behind this handler's back */
    VectorEntry& ve = vectors_[vector];
    ve.gsi = irq;
    ve.level = false;
    __atomic_store_n(&ve.shared, false, __ATOMIC_RELAXED);
    __atomic_store_n(&ve.handlers[0], &handler, __ATOMIC_RELAXED);
    __atomic_store_n(&ve.handler_count, static_cast<u8>(1), __ATOMIC_RELEASE);

    handler.on_interrupt_register(irq, vector);
    return true;
}

bool Interrupt::register_level(InterruptHandler& handler, u8 irq, u8 vector)
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
                klog(0, "Register level interrupt irq 0x%lX: gsi 0x%lX owned by edge vector 0x%lX, refused",
                    (ulong)irq, (ulong)gsi, v);
                return false;
            }

            /* GSI already registered -- chain this handler onto the existing vector */
            const u8 count = ve.handler_count;
            if (count >= MAX_SHARED_HANDLERS)
            {
                klog(0, "Register level interrupt irq 0x%lX: too many shared handlers", (ulong)irq);
                return false;
            }

            /* The handler told its vector before an interrupt can reach it,
               and stored before the count that lets one */
            handler.on_interrupt_register(irq, (u8)v);
            __atomic_store_n(&ve.handlers[count], &handler, __ATOMIC_RELAXED);
            __atomic_store_n(&ve.handler_count, static_cast<u8>(count + 1), __ATOMIC_RELEASE);

            /* Switch to shared dispatch stub */
            __atomic_store_n(&ve.shared, true, __ATOMIC_RELEASE);
            Idt::get_instance().set_descriptor((u8)v, IdtDescriptor::encode(shared_interrupt_stub));

            klog(0, "Register shared level interrupt irq 0x%lX vector 0x%lX handler %lu",
                (ulong)irq, (ulong)v, (ulong)count + 1);
            return true;
        }
    }

    /* Fresh GSI: the requested vector must be free, or we would silently
       overwrite another device's IDT entry */
    if (vectors_[vector].handler_count > 0)
    {
        klog(0, "Register level interrupt irq 0x%lX: vector 0x%lX already in use, refused",
            (ulong)irq, (ulong)vector);
        return false;
    }

    /* And the GSI a pin the IO-APIC has: see register_handler */
    if (!IoApic::get_instance().has_pin(gsi))
    {
        klog(0, "Register level interrupt irq 0x%lX: gsi 0x%lX is no pin of the IO-APIC's, refused",
            (ulong)irq, (ulong)gsi);
        return false;
    }

    klog(0, "Register level interrupt irq 0x%lX gsi 0x%lX vector 0x%lX fn 0x%p active_high %lu",
        (ulong)irq, (ulong)gsi, (ulong)vector, handler.get_handler_fn(), (ulong)active_high);

    /* The vector's handler in place -- its entry here, the handler told,
       its stub in the IDT -- before the pin is unmasked: a line asserted
       already interrupts at once, and used to find the vector nobody's */
    VectorEntry& ve = vectors_[vector];
    ve.gsi = gsi;
    ve.level = true;
    __atomic_store_n(&ve.shared, false, __ATOMIC_RELAXED);
    __atomic_store_n(&ve.handlers[0], &handler, __ATOMIC_RELAXED);
    __atomic_store_n(&ve.handler_count, static_cast<u8>(1), __ATOMIC_RELEASE);
    handler.on_interrupt_register(irq, vector);

    /* First handler -- use device's own stub */
    Idt::get_instance().set_descriptor(vector, IdtDescriptor::encode(handler.get_handler_fn()));

    ulong cpu = IrqBalance::get_instance().assign_io_apic_irq(gsi);
    BUG_ON(!IoApic::get_instance().set_irq_level(gsi, cpu, vector, active_high));
    return true;
}

void Interrupt::unregister_level(InterruptHandler& handler)
{
    for (ulong v = 0; v < MAX_VECTORS; v++)
    {
        VectorEntry& ve = vectors_[v];
        const u8 count = ve.handler_count;
        if (count == 0 || !ve.level)
            continue;

        for (u8 i = 0; i < count; i++)
        {
            if (ve.handlers[i] != &handler)
                continue;

            if (count > 1)
            {
                /* Others stay on the line: the last of them into this one's
                   place, then one fewer. A dispatch that read the count
                   before finds every handler it reaches still there to call
                   -- the last one twice, at worst, which a handler of a
                   shared line takes anyway. The vector stays the shared
                   stub's, whoever is left. */
                __atomic_store_n(&ve.handlers[i], ve.handlers[count - 1], __ATOMIC_RELAXED);
                __atomic_store_n(&ve.handler_count, static_cast<u8>(count - 1), __ATOMIC_RELEASE);
                klog(0, "Unregister level interrupt gsi 0x%lX vector 0x%lX: %lu handlers left",
                    (ulong)ve.gsi, v, (ulong)count - 1);
                return;
            }

            /* The line's last: masked before anything else, so that nothing
               more comes from it; then forgotten by the balancer, which
               would go on retargeting the pin, and the vector given back --
               its IDT entry the one of a vector nobody has, where an
               interrupt already on its way is named and dismissed. */
            const u8 gsi = ve.gsi;
            IoApic::get_instance().mask_irq(gsi);
            IrqBalance::get_instance().remove_io_apic_irq(gsi);
            __atomic_store_n(&ve.handler_count, static_cast<u8>(0), __ATOMIC_RELEASE);
            __atomic_store_n(&ve.shared, false, __ATOMIC_RELEASE);
            Idt::get_instance().reset_descriptor((u8)v);
            ve.level = false;
            ve.gsi = 0;
            klog(0, "Unregister level interrupt gsi 0x%lX vector 0x%lX: masked", (ulong)gsi, v);
            return;
        }
    }

    klog(0, "Unregister level interrupt: handler 0x%p is on no line", &handler);
}

void Interrupt::shared_dispatch(Context* ctx)
{
    /* Its records (tp::Irq), by the first of the shared vectors in service
       -- which the loop below finds again: looked for only when they are on,
       an ISR read a shared vector */
    tp::Irq irq;
    if (unlikely(tp::Irq::on()))
    {
        u32 vector = 0;
        for (ulong v = 0; v < MAX_VECTORS && vector == 0; v++)
        {
            if (__atomic_load_n(&vectors_[v].shared, __ATOMIC_ACQUIRE) && hal::irq_is_in_service((u8)v))
                vector = (u32)v;
        }
        irq.begin(vector, tp::IrqKind::Shared);
    }

    InterruptStats::inc(IrqShared);

    for (ulong v = 0; v < MAX_VECTORS; v++)
    {
        VectorEntry& ve = vectors_[v];
        /* Only dispatch the vector that is actually in service; handlers
           of unrelated shared vectors must not see phantom interrupts.
           A vector the shared stub was installed at, however many of its
           handlers are left (unregister_level): the count read once, and
           every handler it counts one whose pointer is there to call. */
        if (__atomic_load_n(&ve.shared, __ATOMIC_ACQUIRE) && hal::irq_is_in_service((u8)v))
        {
            const u8 count = __atomic_load_n(&ve.handler_count, __ATOMIC_ACQUIRE);
            for (u8 i = 0; i < count && i < MAX_SHARED_HANDLERS; i++)
            {
                InterruptHandler* h = __atomic_load_n(&ve.handlers[i], __ATOMIC_RELAXED);
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