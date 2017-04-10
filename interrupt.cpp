#include "interrupt.h"
#include "idt.h"
#include "cpu.h"
#include "ioapic.h"
#include "trace.h"

namespace Kernel
{

namespace Core
{
    void Interrupt::Register(InterruptHandler& handler, u8 irq, u8 vector)
    {
        Trace(0, "Register interrupt irq 0x%p vector 0x%p fn 0x%p",
            (ulong)irq, (ulong)vector, handler.GetHandlerFn());

        IoApic::GetInstance().SetIrq(irq, CpuTable::GetInstance().GetCurrentCpuId(), vector);
        Idt::GetInstance().SetDescriptor(vector, IdtDescriptor::Encode(handler.GetHandlerFn()));

        handler.OnInterruptRegister(irq, vector);
    }

}
}