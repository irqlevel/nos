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

/* "irq" is the GIC INTID and is the identity here; the x86-style
   "vector" argument is advisory and ignored (callers like the Rust FFI
   pass an x86 vector-space value). OnInterruptRegister reports the
   INTID back as the effective vector. */
void Interrupt::Register(InterruptHandler& handler, u8 irq, u8 vector)
{
    (void)vector;
    RegisterCommon(handler, irq, true, false);
}

void Interrupt::RegisterLevel(InterruptHandler& handler, u8 irq, u8 vector)
{
    (void)vector;
    RegisterCommon(handler, irq, false, true);
}

void Interrupt::SharedDispatch(Context* ctx)
{
    (void)ctx; /* dispatch happens in ArmIrqEntry */
}

extern "C" void ArmIrqEntry(Context* ctx)
{
    bool handled = false;

    for (;;)
    {
        u32 intId = Gic::ReadIar();
        /* The special INTIDs 1020..1023 mean "no (further) interrupt to
           acknowledge" — 1023 is the normal drain-loop terminator. Bound the
           check to that range: LPIs (>= LpiIntIdBase, i.e. 8192) are numerically
           above it and MUST fall through to the LPI dispatch below, otherwise
           they are acknowledged (IAR read raises the running priority) but
           never EOI'd, wedging the CPU interface against every lower/equal
           priority interrupt. */
        if (intId >= Gic::SpuriousIntId && intId < Its::LpiIntIdBase)
        {
            /* An empty first read is a real spurious interrupt; an empty
               re-read is just the dispatch loop draining */
            if (!handled)
                InterruptStats::Inc(IrqSpurious);
            return;
        }
        handled = true;

        if (intId == CpuTable::IPIVector)
        {
            /* EOIs itself (Hal::IrqEoi in Cpu::IPI) before Schedule */
            IPInterrupt(ctx);
            continue;
        }

        if (intId >= Its::LpiIntIdBase)
        {
            Its::GetInstance().HandleLpi(intId);
            Gic::WriteEoir(intId);
            continue;
        }

        if (intId == GenericTimer::GetInstance().IntId())
        {
            /* Per-CPU scheduler tick: LocalTick EOIs before it may
               Schedule() away, so it is not part of the generic path. */
            GenericTimer::GetInstance().LocalTick(ctx);
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
