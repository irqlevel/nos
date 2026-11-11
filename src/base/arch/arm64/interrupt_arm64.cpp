#include "gicv3.h"
#include "its.h"
#include "generic_timer.h"
#include "board.h"

#include <kernel/interrupt.h>
#include <kernel/cpu.h>
#include <kernel/panic.h>
#include <kernel/klog.h>
#include <kernel/tracepoint.h>
#include <hal/context.h>
#include <hal/irqchip.h>

/* arm64 backend for the kernel::Interrupt registration API (the x86 twin
   is kernel/interrupt.cpp over IDT+IOAPIC). "irq" and "vector" are both
   the GIC INTID here; all INTIDs this kernel uses fit the u8 API
   (SGI 1, PPI 27, UART 33, RTC 34, virtio-mmio 48..79).

   IAR/EOIR convention: arm_irq_entry owns the pairing — it EOIs after the
   handlers return. The one exception is the IPI, whose handler chain
   (Cpu::ipi) EOIs itself before it may schedule() away, exactly like the
   x86 flow. */

namespace kernel
{

extern "C" void ip_interrupt(Context* ctx); /* kernel/cpu.cpp */

namespace
{

const ulong MAX_INT_IDS = 256;
const ulong MAX_SHARED_PER_INT_ID = 8;

struct IntIdEntry
{
    u8 handler_count;
    bool level;
    InterruptHandler* handlers[MAX_SHARED_PER_INT_ID];
};

IntIdEntry int_ids[MAX_INT_IDS];

ulong read_mpidr()
{
    ulong mpidr;
    asm volatile("mrs %0, mpidr_el1" : "=r"(mpidr));
    return mpidr;
}

bool register_common(InterruptHandler& handler, u8 int_id, bool edge, bool level)
{
    auto& gic = Gic::get_instance();
    BUG_ON(!gic.is_ready());

    auto& ve = int_ids[int_id];
    if (BUG_ON(ve.handler_count >= MAX_SHARED_PER_INT_ID))
        return false;
    if (BUG_ON(ve.handler_count != 0 && !ve.level))
        return false;
    ve.level = level;
    __atomic_store_n(&ve.handlers[ve.handler_count], &handler, __ATOMIC_RELAXED);
    /* The handler written before the count that lets an interrupt on another
       CPU reach it -- a level line shared is enabled already. Release: the
       compiler and the CPU both keep the order, which plain stores at -O2
       promise neither. */
    __atomic_store_n(&ve.handler_count, static_cast<u8>(ve.handler_count + 1), __ATOMIC_RELEASE);

    gic.enable_irq(int_id, read_mpidr(), edge);
    handler.on_interrupt_register(int_id, int_id);

    klog(0, "Interrupt: intid %lu registered %s", (ulong)int_id,
        level ? "level" : "edge");
    return true;
}

}

/* "irq" is the GIC INTID and is the identity here; the x86-style
   "vector" argument is advisory and ignored (callers like the Rust FFI
   pass an x86 vector-space value). on_interrupt_register reports the
   INTID back as the effective vector. */
bool Interrupt::register_handler(InterruptHandler& handler, u8 irq, u8 vector)
{
    (void)vector;
    return register_common(handler, irq, true, false);
}

bool Interrupt::register_level(InterruptHandler& handler, u8 irq, u8 vector)
{
    (void)vector;
    return register_common(handler, irq, false, true);
}

bool Interrupt::is_vector_free(u8 vector)
{
    (void)vector;
    return true;
}

void Interrupt::unregister_level(InterruptHandler& handler)
{
    for (ulong id = 0; id < MAX_INT_IDS; id++)
    {
        auto& ve = int_ids[id];
        const u8 count = ve.handler_count;
        if (count == 0 || !ve.level)
            continue;

        for (u8 i = 0; i < count; i++)
        {
            if (ve.handlers[i] != &handler)
                continue;

            /* The line's last: disabled at the GIC before the count says
               nobody is there, so that a device still asserting it does
               not interrupt for ever with nobody to quiet it */
            if (count == 1)
                Gic::get_instance().disable_irq((u32)id);

            /* The last handler into this one's place, then one fewer: a
               dispatch that read the count before finds every handler it
               reaches still there to call */
            __atomic_store_n(&ve.handlers[i], ve.handlers[count - 1], __ATOMIC_RELAXED);
            __atomic_store_n(&ve.handler_count, static_cast<u8>(count - 1), __ATOMIC_RELEASE);

            klog(0, "Interrupt: intid %lu, a handler taken off, %lu left%s", id, (ulong)count - 1,
                (count == 1) ? ": disabled" : "");
            return;
        }
    }

    klog(0, "Interrupt: handler 0x%p is on no line", &handler);
}

void Interrupt::shared_dispatch(Context* ctx)
{
    (void)ctx; /* dispatch happens in arm_irq_entry */
}

extern "C" void arm_irq_entry(Context* ctx)
{
    bool handled = false;

    for (;;)
    {
        u32 int_id = Gic::read_iar();
        /* The special INTIDs 1020..1023 mean "no (further) interrupt to
           acknowledge" — 1023 is the normal drain-loop terminator. Bound the
           check to that range: LPIs (>= LPI_INT_ID_BASE, i.e. 8192) are numerically
           above it and MUST fall through to the LPI dispatch below, otherwise
           they are acknowledged (IAR read raises the running priority) but
           never EOI'd, wedging the CPU interface against every lower/equal
           priority interrupt. */
        if (int_id >= Gic::SPURIOUS_INT_ID && int_id < Its::LPI_INT_ID_BASE)
        {
            /* An empty first read is a real spurious interrupt; an empty
               re-read is just the dispatch loop draining */
            if (!handled)
            {
                /* Its records (tp::Irq), as any interrupt's: one taken for
                   nothing */
                tp::Irq irq;
                if (unlikely(tp::Irq::on()))
                    irq.begin(int_id, tp::IrqKind::Spurious);
                InterruptStats::inc(IrqSpurious);
            }
            return;
        }
        handled = true;

        if (int_id == CpuTable::IPI_VECTOR)
        {
            /* EOIs itself (hal::irq_eoi in Cpu::ipi) before Schedule; its
               records are ip_interrupt's, ended before that too */
            ip_interrupt(ctx);
            continue;
        }

        /* Each interrupt the loop takes, its records (tp::Irq): ended as the
           scope is, at the `continue`, past the EOI */
        if (int_id >= Its::LPI_INT_ID_BASE)
        {
            tp::Irq irq;
            if (unlikely(tp::Irq::on()))
                irq.begin(int_id, tp::IrqKind::Msi);

            Its::get_instance().handle_lpi(int_id);
            Gic::write_eoir(int_id);
            continue;
        }

        if (int_id == GenericTimer::get_instance().int_id())
        {
            /* Per-CPU scheduler tick: local_tick EOIs before it may
               schedule() away, so it is not part of the generic path --
               and its records are its own, ended before that too. */
            GenericTimer::get_instance().local_tick(ctx);
            continue;
        }

        tp::Irq irq;

        /* Registration is bounded to u8 INTIDs, but the GIC can report up
           to 1019 — never index int_ids with an unregistered-range id */
        if (int_id >= MAX_INT_IDS)
        {
            if (unlikely(tp::Irq::on()))
                irq.begin(int_id, tp::IrqKind::Stray);
            InterruptStats::inc(IrqDummy);
            Gic::write_eoir(int_id);
            continue;
        }

        auto& ve = int_ids[int_id];
        /* Acquire, the other half of register_common's release: every handler
           counted is one whose pointer is there to call */
        const u8 count = __atomic_load_n(&ve.handler_count, __ATOMIC_ACQUIRE);
        if (unlikely(tp::Irq::on()))
            irq.begin(int_id, (count == 0) ? tp::IrqKind::Stray : tp::IrqKind::Line);
        if (count == 0)
        {
            InterruptStats::inc(IrqDummy);
        }
        else
        {
            for (u8 i = 0; i < count && i < MAX_SHARED_PER_INT_ID; i++)
                __atomic_load_n(&ve.handlers[i], __ATOMIC_RELAXED)->on_interrupt(ctx);
        }

        Gic::write_eoir(int_id);
    }
}

}
