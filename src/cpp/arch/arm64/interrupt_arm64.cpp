#include "gicv3.h"
#include "its.h"
#include "generic_timer.h"
#include "board.h"

#include <kernel/interrupt.h>
#include <kernel/cpu.h>
#include <kernel/panic.h>
#include <kernel/trace.h>
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

void register_common(InterruptHandler& handler, u8 int_id, bool edge, bool level)
{
    auto& gic = Gic::get_instance();
    BUG_ON(!gic.is_ready());

    auto& ve = int_ids[int_id];
    if (BUG_ON(ve.handler_count >= MAX_SHARED_PER_INT_ID))
        return;
    if (BUG_ON(ve.handler_count != 0 && !ve.level))
        return;
    ve.level = level;
    ve.handlers[ve.handler_count] = &handler;
    /* The handler written before the count that lets an interrupt on another
       CPU reach it -- a level line shared is enabled already. Release: the
       compiler and the CPU both keep the order, which plain stores at -O2
       promise neither. */
    __atomic_store_n(&ve.handler_count, static_cast<u8>(ve.handler_count + 1), __ATOMIC_RELEASE);

    gic.enable_irq(int_id, read_mpidr(), edge);
    handler.on_interrupt_register(int_id, int_id);

    trace(0, "Interrupt: intid %lu registered %s", (ulong)int_id,
        level ? "level" : "edge");
}

}

/* "irq" is the GIC INTID and is the identity here; the x86-style
   "vector" argument is advisory and ignored (callers like the Rust FFI
   pass an x86 vector-space value). on_interrupt_register reports the
   INTID back as the effective vector. */
void Interrupt::register_handler(InterruptHandler& handler, u8 irq, u8 vector)
{
    (void)vector;
    register_common(handler, irq, true, false);
}

void Interrupt::register_level(InterruptHandler& handler, u8 irq, u8 vector)
{
    (void)vector;
    register_common(handler, irq, false, true);
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
                InterruptStats::inc(IrqSpurious);
            return;
        }
        handled = true;

        if (int_id == CpuTable::IPI_VECTOR)
        {
            /* EOIs itself (hal::irq_eoi in Cpu::ipi) before Schedule */
            ip_interrupt(ctx);
            continue;
        }

        if (int_id >= Its::LPI_INT_ID_BASE)
        {
            Its::get_instance().handle_lpi(int_id);
            Gic::write_eoir(int_id);
            continue;
        }

        if (int_id == GenericTimer::get_instance().int_id())
        {
            /* Per-CPU scheduler tick: local_tick EOIs before it may
               schedule() away, so it is not part of the generic path. */
            GenericTimer::get_instance().local_tick(ctx);
            continue;
        }

        /* Registration is bounded to u8 INTIDs, but the GIC can report up
           to 1019 — never index int_ids with an unregistered-range id */
        if (int_id >= MAX_INT_IDS)
        {
            InterruptStats::inc(IrqDummy);
            Gic::write_eoir(int_id);
            continue;
        }

        auto& ve = int_ids[int_id];
        /* Acquire, the other half of register_common's release: every handler
           counted is one whose pointer is there to call */
        const u8 count = __atomic_load_n(&ve.handler_count, __ATOMIC_ACQUIRE);
        if (count == 0)
        {
            InterruptStats::inc(IrqDummy);
        }
        else
        {
            for (u8 i = 0; i < count; i++)
                ve.handlers[i]->on_interrupt(ctx);
        }

        Gic::write_eoir(int_id);
    }
}

}
