#include "generic_timer.h"
#include "board.h"
#include "gicv3.h"

#include <kernel/interrupt.h>
#include <kernel/cpu.h>
#include <kernel/sched.h>
#include <kernel/timer.h>
#include <kernel/watchdog.h>
#include <kernel/profiler.h>
#include <kernel/panic.h>
#include <kernel/trace.h>
#include <kernel/time.h>
#include <hal/irqchip.h>

/* Per-CPU scheduler tick over the ARM generic (virtual) timer: 100 Hz on
   every CPU, the timer PPI banked per-CPU, so each CPU ticks itself and
   schedules independently -- as x86's local APIC timers do. One-shot: armed
   by its compare value for the next tick, or sooner for a task asleep until
   a deadline (Cpu::LocalTimer*). Timekeeping reads CNTVCT directly
   (time_arm64.cpp). */

/* The network layer is Rust (src/rust/net): a receive pass offered from the
   tick, which it takes only with rxpoll=on. */
extern "C" void rust_net_poll_rx();

namespace Kernel
{

namespace
{

u64 ReadCntfrq()
{
    u64 freq;
    asm volatile("mrs %0, cntfrq_el0" : "=r"(freq));
    return freq;
}

u64 ReadCntvct()
{
    u64 cnt;
    asm volatile("isb; mrs %0, cntvct_el0" : "=r"(cnt));
    return cnt;
}

}

void GenericTimer::ArmIn(ulong deltaNs)
{
    u64 freq = ReadCntfrq();

    /* A second ahead at most: the timer is armed for a tick at the latest
       (Cpu::LocalTimerArm), and no further keeps the sum below in range */
    deltaNs = Stdlib::Min(deltaNs, MaxArmNs);

    /* Counts, rounded up, whole seconds and the rest apart: a remainder
       under 2^30 times a frequency the architecture has as a 32-bit field
       stays in a u64. The counter is what GetBootTime reads, so a deadline
       in its counts is never early. */
    u64 secs = deltaNs / Const::NanoSecsInSec;
    u64 rest = deltaNs % Const::NanoSecsInSec;
    u64 counts = secs * freq + (rest * freq + Const::NanoSecsInSec - 1) / Const::NanoSecsInSec;

    asm volatile("msr cntv_cval_el0, %0" :: "r"(ReadCntvct() + counts));
    asm volatile("msr cntv_ctl_el0, %0" :: "r"(1UL)); /* ENABLE, no IMASK */
    /* The timer-output (PPI line) update takes effect only after context
       synchronization — required so the EOI that follows a re-arm cannot
       race a still-asserted level line */
    asm volatile("isb");
}

GenericTimer::GenericTimer()
    : TimerIntId(0)
{
}

GenericTimer::~GenericTimer()
{
}

bool GenericTimer::Setup()
{
    TimerIntId = Board::GetInstance().TimerIntId;

    /* Register the handler object once (the INTID -> handler table is
       global). ArmIrqEntry dispatches the timer PPI to LocalTick directly;
       registration also enables the PPI in the BSP's redistributor and
       arms this CPU's CNTV. The architected timer output is a LEVEL
       source (DT says so too); LocalTick deasserts it by re-arming CNTV
       before the EOI. */
    Interrupt::RegisterLevel(*this, (u8)TimerIntId, (u8)TimerIntId);

    CpuTable::GetInstance().GetCurrentCpu().LocalTimerStart();

    Trace(0, "GenericTimer: per-cpu tick %lu Hz, one-shot, counter %lu Hz, intid %lu",
        TickHz, (ulong)ReadCntfrq(), (ulong)TimerIntId);
    return true;
}

void GenericTimer::ArmSelf()
{
    /* Enable the per-CPU timer PPI in this CPU's own redistributor, then
       arm this CPU's CNTV. Called by each AP during startup. */
    ulong mpidr;
    asm volatile("mrs %0, mpidr_el1" : "=r"(mpidr));
    Gic::GetInstance().EnableIrq(TimerIntId, mpidr, false /* level */);
    CpuTable::GetInstance().GetCurrentCpu().LocalTimerStart();
}

void GenericTimer::LocalTick(Context* ctx)
{
    auto& cpus = CpuTable::GetInstance();
    auto& cpu = cpus.GetCurrentCpu();

    /* Re-armed first, whatever follows: the line is a level, and armed in
       the future it goes down before the EOI */
    if (Panicker::GetInstance().IsActive())
    {
        /* Ticking on, without the walk: the queue's lock may be the
           panicking CPU's */
        cpu.LocalTimerTick();
        Hal::IrqEoi((u8)TimerIntId);
        return;
    }

    if (!cpu.LocalTimerFired(GetBootTime().GetValue()))
    {
        /* A sleeper's deadline, not the tick: only the reschedule */
        InterruptStats::Inc(IrqTimerWake);
        Hal::IrqEoi((u8)TimerIntId);
        Preempt();
        return;
    }

    Watchdog::GetInstance().Check();

    Profiler::GetInstance().Tick(ctx);

    /* Global software timers (Sleep, Rust Timer) are processed on the BSP */
    if (cpu.GetIndex() == cpus.GetBspIndexNoLock())
    {
        TimerTable::GetInstance().ProcessTimers();

        /* And offer the receive path a look, whether or not a NIC asked.
           The network layer takes it only with rxpoll=on -- the switch is
           in its poll, DeviceTable::poll_rx, not here. */
        rust_net_poll_rx();
    }

    /* EOI before Preempt(): it may context-switch away and only return
       when this task is rescheduled, so the PPI must be deactivated first
       (the IPI path does the same for the SGI). Preempt, not Schedule: a
       task that can still run is never handed over to the idle task. */
    Hal::IrqEoi((u8)TimerIntId);

    Preempt();
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
