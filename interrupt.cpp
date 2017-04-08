#include "interrupt.h"
#include "idt.h"
#include "lapic.h"
#include "ioapic.h"
#include "trace.h"

namespace Kernel
{

namespace Core
{
    void Interrupt::Register(InterruptHandler& handler, u8 irq, u8 vector)
    {
        auto& idt = Idt::GetInstance();
        auto& ioApic = IoApic::GetInstance();
        auto& lapic = Lapic::GetInstance();

        Trace(0, "Register interrupt irq 0x%p vector 0x%p fn 0x%p",
            (ulong)irq, (ulong)vector, handler.GetHandlerFn());

        ioApic.SetIrq(irq, lapic.GetId(), vector);
        idt.SetDescriptor(vector, IdtDescriptor::Encode(handler.GetHandlerFn()));

        handler.OnInterruptRegister(irq, vector);
    }

}
}