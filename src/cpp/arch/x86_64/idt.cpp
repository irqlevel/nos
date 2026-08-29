#include "idt.h"
#include "asm.h"
#include <drivers/screen.h>
#include <hal/context.h>
#include <hal/irqchip.h>
#include <kernel/interrupt.h>
#include <kernel/panic.h>
#include <kernel/trace.h>

#include <lib/stdlib.h>

namespace Kernel
{

/* One stub per vector, generated in asm.asm; each passes its own index to
   DummyInterruptDispatch */
extern "C" InterruptHandlerFn DummyInterruptStubTable[256];

/* Vectors whose exception frame carries a CPU-pushed error code (SDM
   vol.3, "Exception- and Interrupt-Vector Reference"). The dummy stub
   does not pop it, so the panic dump has to know it is there. */
static bool VectorHasErrorCode(u8 vector)
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
    : Base(0)
    , Limit(0)
{
    Trace(0, "Idt 0x%p", this);

    static_assert(sizeof(Entry) / sizeof(Entry[0]) == 256,
        "DummyInterruptStubTable holds one stub per idt entry");

    for (size_t i = 0; i < Stdlib::ArraySize(Entry); i++)
    {
        Entry[i] = IdtDescriptor::Encode(DummyInterruptStubTable[i]);
    }
}

Idt::~Idt()
{
}

void Idt::Save()
{
    TableDesc desc = {
        .Limit = sizeof(Entry),
        .Base = reinterpret_cast<u64>(&Entry[0]),
    };

    LoadIdt(&desc);

    Base = desc.Base;
    Limit = desc.Limit;
}

u32 Idt::GetBase()
{
    return Base;
}

u16 Idt::GetLimit()
{
    return Limit;
}

IdtDescriptor Idt::GetDescriptor(u16 index)
{
    if (index >= Stdlib::ArraySize(Entry))
	    return IdtDescriptor();

    return Entry[index];
}

void Idt::SetDescriptor(u16 index, const IdtDescriptor& desc)
{
    if (index >= Stdlib::ArraySize(Entry))
	    return;

    Entry[index] = desc;
}

void Idt::DummyInterrupt(Context* ctx, u8 vector)
{
    DummyInterruptCounter.Inc();

    /* Below FirstDeviceVector this is a CPU exception on a slot
       ExceptionTable never claimed -- a fault before it ran, or one of the
       reserved vectors. That is a kernel bug, and the stub cannot step
       over the error code some of them push, so there is no returning. */
    if (vector < FirstDeviceVector)
    {
        PanicCtx(ctx, VectorHasErrorCode(vector), "Unknown exception, vector 0x%p",
            (ulong)vector);
        return;
    }

    /* A device interrupt on a vector no driver owns is a firmware or
       hardware event, not a broken invariant: real firmware hands over
       LVTs still armed, and a masked 8259 answers an ExtINT INTA cycle
       with its spurious vector. Name it and keep going -- a panic here
       makes a machine unbootable over something Linux logs one line
       about. */
    long count = DummyInterruptCounter.Get();
    if (count <= MaxDummyReports)
    {
        Trace(0, "Unknown interrupt, vector 0x%p rip 0x%p count %u",
            (ulong)vector, ctx->GetRetRip(), (ulong)count);

        /* The screen is the only channel on a machine with no serial port */
        Screen::Printf("Unknown interrupt, vector 0x%p\n", (ulong)vector);
    }

    /* EOI only what the LAPIC actually has in service: a vector delivered
       through LINT0 in ExtINT mode never set an ISR bit, and the EOI would
       then dismiss an unrelated interrupt. */
    if (Hal::IrqIsInService(vector))
        Hal::IrqEoi(vector);
}

extern "C" void DummyInterruptDispatch(Context* ctx, u32 vector)
{
    InterruptStats::Inc(IrqDummy);
    Idt::GetInstance().DummyInterrupt(ctx, (u8)vector);
}

extern "C" void SpuriousInterrupt()
{
    /* LAPIC spurious-interrupt vector (Lapic::SpuriousVector). Per the SDM a
       spurious interrupt must NOT be acknowledged with an EOI -- just count
       it and return, rather than falling through to DummyInterrupt's panic. */
    InterruptStats::Inc(IrqSpurious);
}

}
