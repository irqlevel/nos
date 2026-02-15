#include "interrupt.h"
#include "idt.h"
#include "cpu.h"
#include "trace.h"
#include "asm.h"

#include <drivers/acpi.h>
#include <drivers/ioapic.h>
#include <drivers/lapic.h>

namespace Kernel
{

Atomic InterruptStats::Counters[IrqMax];

void InterruptStats::Inc(InterruptSource src)
{
    if (src < IrqMax)
        Counters[src].Inc();
}

long InterruptStats::Get(InterruptSource src)
{
    if (src < IrqMax)
        return Counters[src].Get();
    return 0;
}

const char* InterruptStats::GetName(InterruptSource src)
{
    switch (src)
    {
    case IrqPit:        return "pit";
    case IrqIO8042:     return "8042";
    case IrqSerial:     return "serial";
    case IrqVirtioBlk:  return "virtio-blk";
    case IrqVirtioNet:  return "virtio-net";
    case IrqVirtioScsi: return "virtio-scsi";
    case IrqIPI:        return "ipi";
    case IrqShared:     return "shared";
    case IrqDummy:      return "dummy";
    default:            return "unknown";
    }
}

Interrupt::VectorEntry Interrupt::Vectors[MaxVectors];

void Interrupt::Register(InterruptHandler& handler, u8 irq, u8 vector)
{
    Trace(0, "Register interrupt irq 0x%p vector 0x%p fn 0x%p",
        (ulong)irq, (ulong)vector, handler.GetHandlerFn());

    IoApic::GetInstance().SetIrq(irq, CpuTable::GetInstance().GetCurrentCpuId(), vector);
    Idt::GetInstance().SetDescriptor(vector, IdtDescriptor::Encode(handler.GetHandlerFn()));

    handler.OnInterruptRegister(irq, vector);
}

void Interrupt::RegisterLevel(InterruptHandler& handler, u8 irq, u8 vector)
{
    auto& acpi = Acpi::GetInstance();
    u8 gsi = (u8)acpi.GetGsiByIrq(irq);

    /* Decode ACPI MADT polarity flags for this IRQ.
       Bits [1:0]: 00=bus default, 01=active high, 11=active low.
       PCI bus default is active low. */
    u16 acpiFlags = acpi.GetIrqFlags(irq);
    u8 polarity = acpiFlags & 3;
    bool activeHigh = (polarity == 1);

    /* Check if this GSI is already registered on another vector */
    for (ulong v = 0; v < MaxVectors; v++)
    {
        VectorEntry& ve = Vectors[v];
        if (ve.HandlerCount > 0 && ve.Gsi == gsi)
        {
            /* GSI already registered -- chain this handler onto the existing vector */
            if (ve.HandlerCount >= MaxSharedHandlers)
            {
                Trace(0, "Register level interrupt irq 0x%p: too many shared handlers", (ulong)irq);
                return;
            }

            ve.Handlers[ve.HandlerCount] = &handler;
            ve.HandlerCount++;

            /* Switch to shared dispatch stub */
            Idt::GetInstance().SetDescriptor((u8)v, IdtDescriptor::Encode(SharedInterruptStub));

            Trace(0, "Register shared level interrupt irq 0x%p vector 0x%p handler %u",
                (ulong)irq, (ulong)v, (ulong)ve.HandlerCount);

            handler.OnInterruptRegister(irq, (u8)v);
            return;
        }
    }

    Trace(0, "Register level interrupt irq 0x%p gsi 0x%p vector 0x%p fn 0x%p activeHigh %u",
        (ulong)irq, (ulong)gsi, (ulong)vector, handler.GetHandlerFn(), (ulong)activeHigh);

    IoApic::GetInstance().SetIrqLevel(gsi, CpuTable::GetInstance().GetCurrentCpuId(), vector, activeHigh);

    /* Record in the shared table */
    VectorEntry& ve = Vectors[vector];
    ve.Gsi = gsi;
    ve.Handlers[0] = &handler;
    ve.HandlerCount = 1;

    /* First handler -- use device's own stub */
    Idt::GetInstance().SetDescriptor(vector, IdtDescriptor::Encode(handler.GetHandlerFn()));

    handler.OnInterruptRegister(irq, vector);
}

void Interrupt::SharedDispatch(Context* ctx)
{
    InterruptStats::Inc(IrqShared);

    for (ulong v = 0; v < MaxVectors; v++)
    {
        VectorEntry& ve = Vectors[v];
        if (ve.HandlerCount > 1)
        {
            for (u8 i = 0; i < ve.HandlerCount; i++)
            {
                InterruptHandler* h = ve.Handlers[i];
                if (h)
                    h->OnInterrupt(ctx);
            }
        }
    }

    Lapic::EOI();
}

}

extern "C" void SharedInterrupt(Kernel::Context* ctx)
{
    Kernel::Interrupt::SharedDispatch(ctx);
}