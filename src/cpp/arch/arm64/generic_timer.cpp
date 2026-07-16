#include "generic_timer.h"
#include "board.h"

#include <kernel/interrupt.h>
#include <kernel/cpu.h>
#include <kernel/trace.h>

/* Scheduler tick over the ARM generic (virtual) timer: 100 Hz on the BSP,
   broadcast to every CPU as an IPI — exactly the PIT/HPET pattern on x86
   (drivers/pit.cpp), so the scheduler logic is unchanged. Timekeeping
   itself reads CNTVCT directly (time_arm64.cpp) and needs no tick. */

namespace Kernel
{

namespace
{

const ulong TickHz = 100;

u64 ReadCntfrq()
{
    u64 freq;
    asm volatile("mrs %0, cntfrq_el0" : "=r"(freq));
    return freq;
}

void ArmTimer(u64 ticks)
{
    asm volatile("msr cntv_tval_el0, %0" :: "r"(ticks));
    asm volatile("msr cntv_ctl_el0, %0" :: "r"(1UL)); /* ENABLE, no IMASK */
}

}

GenericTimer::GenericTimer()
    : IntVector(0)
    , TickInterval(0)
{
}

GenericTimer::~GenericTimer()
{
}

bool GenericTimer::Setup()
{
    TickInterval = ReadCntfrq() / TickHz;

    u32 intId = Board::GetInstance().TimerIntId;
    Interrupt::Register(*this, intId, intId);

    ArmTimer(TickInterval);

    Trace(0, "GenericTimer: tick %u Hz interval %u intid %u",
        TickHz, TickInterval, (ulong)intId);
    return true;
}

void GenericTimer::OnInterruptRegister(u8 irq, u8 vector)
{
    (void)irq;
    IntVector = vector;
}

InterruptHandlerFn GenericTimer::GetHandlerFn()
{
    return nullptr; /* dispatch is object-based on arm64 */
}

void GenericTimer::OnInterrupt(Context* ctx)
{
    (void)ctx;

    ArmTimer(TickInterval);

    /* Broadcast the scheduling tick; the EOI is done by the dispatch
       loop after this handler returns, before the SGI can preempt. */
    CpuTable::GetInstance().SendIPIAll();
}

}
