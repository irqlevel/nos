#include "gicv3.h"

#include <kernel/interrupt.h>
#include <kernel/cpu.h>
#include <kernel/panic.h>
#include <kernel/trace.h>
#include <hal/context.h>
#include <hal/irqchip.h>

/* arm64 backend for the Kernel::Interrupt registration API (the x86 twin
   is kernel/interrupt.cpp over IDT+IOAPIC). "irq" and "vector" are both
   the GIC INTID here; all INTIDs this kernel uses fit the u8 API
   (SGI 1, PPI 27, UART 33, RTC 34, virtio-mmio 48..79).

   IAR/EOIR convention: ArmIrqEntry owns the pairing — it EOIs after the
   handlers return. The one exception is the IPI, whose handler chain
   (Cpu::IPI) EOIs itself before it may Schedule() away, exactly like the
   x86 flow. */

namespace Kernel
{

extern "C" void IPInterrupt(Context* ctx); /* kernel/cpu.cpp */

namespace
{

const ulong MaxIntIds = 256;
const ulong MaxSharedPerIntId = 8;

struct IntIdEntry
{
    u8 HandlerCount;
    bool Level;
    InterruptHandler* Handlers[MaxSharedPerIntId];
};

IntIdEntry IntIds[MaxIntIds];

ulong ReadMpidr()
{
    ulong mpidr;
    asm volatile("mrs %0, mpidr_el1" : "=r"(mpidr));
    return mpidr;
}

void RegisterCommon(InterruptHandler& handler, u8 intId, bool edge, bool level)
{
    auto& gic = Gic::GetInstance();
    BugOn(!gic.IsReady());

    auto& ve = IntIds[intId];
    if (BugOn(ve.HandlerCount >= MaxSharedPerIntId))
        return;
    if (BugOn(ve.HandlerCount != 0 && !ve.Level))
        return;
    ve.Level = level;
    ve.Handlers[ve.HandlerCount] = &handler;
    ve.HandlerCount = ve.HandlerCount + 1;

    gic.EnableIrq(intId, ReadMpidr(), edge);
    handler.OnInterruptRegister(intId, intId);

    Trace(0, "Interrupt: intid %u registered %s", (ulong)intId,
        level ? "level" : "edge");
}

}

void Interrupt::Register(InterruptHandler& handler, u8 irq, u8 vector)
{
    BugOn(irq != vector);
    RegisterCommon(handler, vector, true, false);
}

void Interrupt::RegisterLevel(InterruptHandler& handler, u8 irq, u8 vector)
{
    BugOn(irq != vector);
    RegisterCommon(handler, vector, false, true);
}

void Interrupt::SharedDispatch(Context* ctx)
{
    (void)ctx; /* dispatch happens in ArmIrqEntry */
}

extern "C" void ArmIrqEntry(Context* ctx)
{
    for (;;)
    {
        u32 intId = Gic::ReadIar();
        if (intId >= Gic::SpuriousIntId)
        {
            InterruptStats::Inc(IrqSpurious);
            return;
        }

        if (intId == CpuTable::IPIVector)
        {
            /* EOIs itself (Hal::IrqEoi in Cpu::IPI) before Schedule */
            IPInterrupt(ctx);
            continue;
        }

        auto& ve = IntIds[intId];
        if (ve.HandlerCount == 0)
        {
            InterruptStats::Inc(IrqDummy);
        }
        else
        {
            for (u8 i = 0; i < ve.HandlerCount; i++)
                ve.Handlers[i]->OnInterrupt(ctx);
        }

        Gic::WriteEoir(intId);
    }
}

}
