#include "generic_timer.h"
#include "board.h"
#include "gicv3.h"

#include <kernel/interrupt.h>
#include <kernel/cpu.h>
#include <kernel/sched.h>
#include <kernel/timer.h>
#include <kernel/watchdog.h>
#include <kernel/panic.h>
#include <kernel/trace.h>
#include <hal/irqchip.h>

/* Per-CPU scheduler tick over the ARM generic (virtual) timer: 100 Hz on
   every CPU. The x86 tick fires on the BSP and broadcasts via SGI
   (drivers/pit.cpp); here the timer PPI is banked per-CPU, so each CPU
   ticks itself — no per-tick IPI broadcast, and each CPU schedules
   independently. Timekeeping reads CNTVCT directly (time_arm64.cpp). */

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
    : TimerIntId(0)
    , TickInterval(0)
{
}

GenericTimer::~GenericTimer()
{
}

bool GenericTimer::Setup()
{
    TickInterval = ReadCntfrq() / TickHz;
    TimerIntId = Board::GetInstance().TimerIntId;

    /* Register the handler object once (the INTID -> handler table is
       global). ArmIrqEntry dispatches the timer PPI to LocalTick directly;
       registration also enables the PPI in the BSP's redistributor and
       arms this CPU's CNTV. */
    Interrupt::Register(*this, (u8)TimerIntId, (u8)TimerIntId);

    ArmTimer(TickInterval);

    Trace(0, "GenericTimer: per-cpu tick %u Hz interval %u intid %u",
        TickHz, TickInterval, (ulong)TimerIntId);
    return true;
}

void GenericTimer::ArmSelf()
{
    /* Enable the per-CPU timer PPI in this CPU's own redistributor, then
       arm this CPU's CNTV. Called by each AP during startup. */
    ulong mpidr;
    asm volatile("mrs %0, mpidr_el1" : "=r"(mpidr));
    Gic::GetInstance().EnableIrq(TimerIntId, mpidr, true);
    ArmTimer(TickInterval);
}

void GenericTimer::LocalTick(Context* ctx)
{
    (void)ctx;

    ArmTimer(TickInterval);

    if (Panicker::GetInstance().IsActive())
    {
        Hal::IrqEoi((u8)TimerIntId);
        return;
    }

    auto& cpus = CpuTable::GetInstance();
    auto& cpu = cpus.GetCurrentCpu();

    Watchdog::GetInstance().Check();

    /* Global software timers (Sleep, Rust Timer) are processed on the BSP */
    if (cpu.GetIndex() == cpus.GetBspIndexNoLock())
        TimerTable::GetInstance().ProcessTimers();

    /* EOI before Schedule(): it may context-switch away and only return
       when this task is rescheduled, so the PPI must be deactivated first
       (the IPI path does the same for the SGI). */
    Hal::IrqEoi((u8)TimerIntId);

    Schedule();
}

void GenericTimer::OnInterruptRegister(u8 irq, u8 vector)
{
    (void)irq;
    (void)vector;
}

InterruptHandlerFn GenericTimer::GetHandlerFn()
{
    return nullptr; /* dispatch is object-based on arm64 */
}

void GenericTimer::OnInterrupt(Context* ctx)
{
    /* Unused: ArmIrqEntry routes the timer PPI to LocalTick directly so it
       can EOI before Schedule. Kept for the InterruptHandler contract. */
    LocalTick(ctx);
}

}
