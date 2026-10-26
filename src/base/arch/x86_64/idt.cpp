#include "idt.h"
#include "asm.h"
#include <drivers/screen.h>
#include <hal/context.h>
#include <hal/irqchip.h>
#include <kernel/interrupt.h>
#include <kernel/panic.h>
#include <kernel/trace.h>

#include <lib/stdlib.h>

namespace kernel
{

/* One stub per vector, generated in asm.asm; each passes its own index to
   dummy_interrupt_dispatch */
extern "C" InterruptHandlerFn dummy_interrupt_stub_table[256];

/* Vectors whose exception frame carries a CPU-pushed error code (SDM
   vol.3, "Exception- and Interrupt-Vector Reference"). The dummy stub
   does not pop it, so the panic dump has to know it is there. */
static bool vector_has_error_code(u8 vector)
{
    switch (vector)
    {
    case 0x8:  // #DF
    case 0xA:  // #TS
    case 0xB:  // #NP
    case 0xC:  // #SS
    case 0xD:  // #GP
    case 0xE:  // #PF
    case 0x11: // #AC
    case 0x15: // #CP
        return true;
    default:
        return false;
    }
}

Idt::Idt()
    : base_(0)
    , limit_(0)
{
    trace(0, "Idt 0x%p", this);

    static_assert(sizeof(entry_) / sizeof(entry_[0]) == 256,
        "dummy_interrupt_stub_table holds one stub per idt entry");

    for (size_t i = 0; i < stdlib::array_size(entry_); i++)
    {
        entry_[i] = IdtDescriptor::encode(dummy_interrupt_stub_table[i]);
    }
}

Idt::~Idt()
{
}

void Idt::save()
{
    TableDesc desc = {
        .limit = sizeof(entry_),
        .base = reinterpret_cast<u64>(&entry_[0]),
    };

    load_idt(&desc);

    base_ = desc.base;
    limit_ = desc.limit;
}

u32 Idt::get_base()
{
    return base_;
}

u16 Idt::get_limit()
{
    return limit_;
}

IdtDescriptor Idt::get_descriptor(u16 index)
{
    if (index >= stdlib::array_size(entry_))
	    return IdtDescriptor();

    return entry_[index];
}

void Idt::set_descriptor(u16 index, const IdtDescriptor& desc)
{
    if (index >= stdlib::array_size(entry_))
	    return;

    entry_[index] = desc;
}

void Idt::dummy_interrupt(Context* ctx, u8 vector)
{
    dummy_interrupt_counter_.inc();

    /* Below FIRST_DEVICE_VECTOR this is a CPU exception on a slot
       ExceptionTable never claimed -- a fault before it ran, or one of the
       reserved vectors. That is a kernel bug, and the stub cannot step
       over the error code some of them push, so there is no returning. */
    if (vector < FIRST_DEVICE_VECTOR)
    {
        PANIC_CTX(ctx, vector_has_error_code(vector), "Unknown exception, vector 0x%lX",
            (ulong)vector);
        return;
    }

    /* A device interrupt on a vector no driver owns is a firmware or
       hardware event, not a broken invariant: real firmware hands over
       LVTs still armed, and a masked 8259 answers an ExtINT INTA cycle
       with its spurious vector. Name it and keep going -- a panic here
       makes a machine unbootable over something Linux logs one line
       about. */
    long count = dummy_interrupt_counter_.get();
    if (count <= MAX_DUMMY_REPORTS)
    {
        trace(0, "Unknown interrupt, vector 0x%lX rip 0x%lX count %lu",
            (ulong)vector, ctx->get_ret_rip(), (ulong)count);

        /* The screen is the only channel on a machine with no serial port */
        screen::printf("Unknown interrupt, vector 0x%lX\n", (ulong)vector);
    }

    /* EOI only what the LAPIC actually has in service: a vector delivered
       through LINT0 in ExtINT mode never set an ISR bit, and the EOI would
       then dismiss an unrelated interrupt. */
    if (hal::irq_is_in_service(vector))
        hal::irq_eoi(vector);
}

extern "C" void dummy_interrupt_dispatch(Context* ctx, u32 vector)
{
    InterruptStats::inc(IrqDummy);
    Idt::get_instance().dummy_interrupt(ctx, (u8)vector);
}

extern "C" void spurious_interrupt()
{
    /* LAPIC spurious-interrupt vector (Lapic::SPURIOUS_VECTOR). Per the SDM a
       spurious interrupt must NOT be acknowledged with an EOI -- just count
       it and return, rather than falling through to dummy_interrupt's panic. */
    InterruptStats::inc(IrqSpurious);
}

}
