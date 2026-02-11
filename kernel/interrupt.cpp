#include "interrupt.h"
#include "idt.h"
#include "cpu.h"
#include "trace.h"

#include <drivers/ioapic.h>

namespace Kernel
{

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
    Trace(0, "Register level interrupt irq 0x%p vector 0x%p fn 0x%p",
        (ulong)irq, (ulong)vector, handler.GetHandlerFn());

    IoApic::GetInstance().SetIrqLevel(irq, CpuTable::GetInstance().GetCurrentCpuId(), vector);
    Idt::GetInstance().SetDescriptor(vector, IdtDescriptor::Encode(handler.GetHandlerFn()));

    handler.OnInterruptRegister(irq, vector);
}

}