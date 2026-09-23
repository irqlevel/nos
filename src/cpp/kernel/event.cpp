#include "event.h"
#include "time.h"
#include "task.h"
#include "sched.h"
#include "cpu.h"
#include "panic.h"

#include <hal/cpu.h>
#include <hal/irqchip.h>

namespace Kernel
{

Event::Event()
    : Signaled(0)
    , Waiter(0)
    , WaiterCpu(0)
{
}

Event::~Event()
{
    BugOn(Waiter.Get() != 0);
}

void Event::Wait()
{
    Task* self = Task::GetCurrentTask();

    /* One waiter at a time, claimed for the whole wait: a second would take
       over the slot, and each Signal() would then wake one of the two. */
    BugOn(Waiter.Cmpxchg((long)self, 0) != 0);

    for (;;)
    {
        /* A signal already there is taken without blocking at all -- which
           is the common case for a waiter that is busy when it is signalled. */
        if (Signaled.Cmpxchg(0, 1) == 1)
            break;

        /* Interrupts off from the moment the flag goes up until it is down
           again. The tick and every IPI end in a Schedule() of their own, and
           one landing while the flag is up switches the task out as a
           blocked one: asleep on a signal it never looked at, or had just
           seen, which nothing may ever send again. That is not a theory -- it
           is how the block server's worker went to sleep for good with fifty
           requests queued. The task comes back from its own Schedule(), when
           it is woken, with interrupts still off: Schedule() saves and
           restores them per task. */
        ulong flags = Hal::IrqSave();

        WaiterCpu.Set((long)CpuTable::GetInstance().GetCurrentCpuId());
        self->Block();

        if (Signaled.Get() == 0)
            Schedule();

        /* Running again: the signaller cleared the flag, the re-check found
           the signal, or Schedule() had nobody else to run. Down before
           interrupts come back on -- a tick held off until the IrqRestore
           would find it still up. */
        self->Unblock();

        Hal::IrqRestore(flags);
    }

    Waiter.Set(0);
}

bool Event::WaitFor(unsigned long long timeoutNs)
{
    Task* self = Task::GetCurrentTask();

    /* One waiter at a time, as in Wait() */
    BugOn(Waiter.Cmpxchg((long)self, 0) != 0);

    ulong start = GetBootTime().GetValue();
    ulong until = (timeoutNs > ~0UL - start) ? ~0UL : start + (ulong)timeoutNs;
    bool signaled = false;

    for (;;)
    {
        if (Signaled.Cmpxchg(0, 1) == 1)
        {
            signaled = true;
            break;
        }
        if (GetBootTime().GetValue() >= until)
            break;

        /* Wait()'s window, interrupts off from the flag going up until it is
           down again, with the deadline beside it for the scheduler: a
           Signal() unblocks the task, and so does its time coming. */
        ulong flags = Hal::IrqSave();

        WaiterCpu.Set((long)CpuTable::GetInstance().GetCurrentCpuId());
        self->SleepUntil.Set((long)until);
        self->Block();

        if (Signaled.Get() == 0)
            Schedule();

        self->Unblock();
        self->SleepUntil.Set(0);

        Hal::IrqRestore(flags);
    }

    Waiter.Set(0);
    return signaled;
}

void Event::Signal()
{
    /* Published first. A plain read before the locked operation, so a burst
       of signals to a busy waiter costs one locked instruction, not one
       each. */
    if (Signaled.Get() == 0)
        Signaled.Cmpxchg(1, 0);

    /* Then the waiter, every time -- not only by whoever set the flag: a
       waiter found blocked with a signal pending is one to wake, whoever's
       signal it was. */
    Task* waiter = (Task*)Waiter.Get();
    if (waiter == nullptr || !waiter->IsBlocked())
        return;

    waiter->Unblock();

    /* Unblocked is not running: an idle CPU would not look at its queue again
       until the next tick. The IPI ends in Schedule(), which finds the task
       runnable -- the same kick SoftIrq::Raise gives its tasks. */
    Hal::SendIpi((ulong)WaiterCpu.Get(), CpuTable::IPIVector);
}

}
