#include "event.h"
#include "time.h"
#include "task.h"
#include "sched.h"
#include "cpu.h"
#include "panic.h"

#include <hal/cpu.h>
#include <hal/irqchip.h>

namespace kernel
{

Event::Event()
    : signaled_(0)
    , waiter_(0)
    , waiter_cpu_(0)
{
}

Event::~Event()
{
    BUG_ON(waiter_.get() != 0);
}

void Event::wait()
{
    Task* self = Task::get_current_task();

    /* One waiter at a time, claimed for the whole wait: a second would take
       over the slot, and each Signal() would then wake one of the two. */
    BUG_ON(waiter_.cmpxchg((long)self, 0) != 0);

    for (;;)
    {
        /* A signal already there is taken without blocking at all -- which
           is the common case for a waiter that is busy when it is signalled. */
        if (signaled_.cmpxchg(0, 1) == 1)
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
        ulong flags = hal::irq_save();

        waiter_cpu_.set((long)CpuTable::get_instance().get_current_cpu_id());
        self->block();

        if (signaled_.get() == 0)
            schedule();

        /* Running again: the signaller cleared the flag, the re-check found
           the signal, or Schedule() had nobody else to run. Down before
           interrupts come back on -- a tick held off until the IrqRestore
           would find it still up. */
        self->unblock();

        hal::irq_restore(flags);
    }

    waiter_.set(0);
}

bool Event::wait_for(unsigned long long timeout_ns)
{
    Task* self = Task::get_current_task();

    /* One waiter at a time, as in Wait() */
    BUG_ON(waiter_.cmpxchg((long)self, 0) != 0);

    ulong start = get_boot_time().get_value();
    ulong until = (timeout_ns > ~0UL - start) ? ~0UL : start + (ulong)timeout_ns;
    bool signaled = false;

    for (;;)
    {
        if (signaled_.cmpxchg(0, 1) == 1)
        {
            signaled = true;
            break;
        }
        if (get_boot_time().get_value() >= until)
            break;

        /* Wait()'s window, interrupts off from the flag going up until it is
           down again, with the deadline beside it for the scheduler: a
           Signal() unblocks the task, and so does its time coming. */
        ulong flags = hal::irq_save();

        waiter_cpu_.set((long)CpuTable::get_instance().get_current_cpu_id());
        self->sleep_until_.set((long)until);
        self->block();

        if (signaled_.get() == 0)
            schedule();

        self->unblock();
        self->sleep_until_.set(0);

        hal::irq_restore(flags);
    }

    waiter_.set(0);
    return signaled;
}

void Event::signal()
{
    /* Published first. A plain read before the locked operation, so a burst
       of signals to a busy waiter costs one locked instruction, not one
       each. */
    if (signaled_.get() == 0)
        signaled_.cmpxchg(1, 0);

    /* Then the waiter, every time -- not only by whoever set the flag: a
       waiter found blocked with a signal pending is one to wake, whoever's
       signal it was. */
    Task* waiter = (Task*)waiter_.get();
    if (waiter == nullptr || !waiter->is_blocked())
        return;

    waiter->unblock();

    /* Unblocked is not running: an idle CPU would not look at its queue again
       until the next tick. The IPI ends in Schedule(), which finds the task
       runnable -- the same kick SoftIrq::Raise gives its tasks. */
    hal::send_ipi((ulong)waiter_cpu_.get(), CpuTable::IPI_VECTOR);
}

}
