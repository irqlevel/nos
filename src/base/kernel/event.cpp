#include "event.h"
#include "time.h"
#include "task.h"
#include "sched.h"
#include "cpu.h"
#include "panic.h"

#include <hal/barrier.h>
#include <hal/cpu.h>
#include <hal/irqchip.h>

namespace kernel
{

/* The waits that met a signal() in its window as they let go of their task
   (Event::let_go_waits) */
static Atomic let_go_met;

Event::Event()
    : signaled_(0)
    , waiter_(0)
    , waiter_cpu_(0)
    , signallers_(0)
{
}

Event::~Event()
{
    BUG_ON(waiter_.get() != 0);
    /* A signal() still running: the owner's bug, which a free of the event
       turns into a write to freed memory */
    BUG_ON(signallers_.get() != 0);
}

void Event::wait()
{
    Task* self = Task::get_current_task();

    /* One waiter at a time, claimed for the whole wait: a second would take
       over the slot, and each signal() would then wake one of the two. */
    BUG_ON(waiter_.cmpxchg((long)self, 0) != 0);

    /* A CPU's idle task waits without leaving the scheduler's walk: it is
       the last resort, switched to when nothing else on its CPU can run,
       and found asleep there it was a BUG_ON in the scheduler, far from the
       wait that put it to sleep -- the boot's own task is one, and what it
       calls is not always its to know (a module's command waiting on a
       mailbox). It lets whoever can run have the CPU and looks again, as
       sleep() does there. */
    const bool may_block = !self->is_idle();

    for (;;)
    {
        /* A signal already there is taken without blocking at all -- which
           is the common case for a waiter that is busy when it is signalled. */
        if (signaled_.cmpxchg(0, 1) == 1)
            break;

        if (unlikely(!may_block))
        {
            schedule();
            continue;
        }

        /* Interrupts off from the moment the flag goes up until it is down
           again. The tick and every IPI end in a schedule() of their own, and
           one landing while the flag is up switches the task out as a
           blocked one: asleep on a signal it never looked at, or had just
           seen, which nothing may ever send again. That is not a theory -- it
           is how the block server's worker went to sleep for good with fifty
           requests queued. The task comes back from its own schedule(), when
           it is woken, with interrupts still off: schedule() saves and
           restores them per task. */
        ulong flags = hal::irq_save();

        waiter_cpu_.set((long)CpuTable::get_instance().get_current_cpu_id());
        self->block();

        if (signaled_.get() == 0)
            schedule();

        /* Running again: the signaller cleared the flag, the re-check found
           the signal, or schedule() had nobody else to run. Down before
           interrupts come back on -- a tick held off until the irq_restore
           would find it still up. */
        self->unblock();

        hal::irq_restore(flags);
    }

    let_go();
}

bool Event::wait_for(unsigned long long timeout_ns)
{
    Task* self = Task::get_current_task();

    /* One waiter at a time, as in wait() */
    BUG_ON(waiter_.cmpxchg((long)self, 0) != 0);

    ulong start = get_boot_time().get_value();
    ulong until = (timeout_ns > ~0UL - start) ? ~0UL : start + (ulong)timeout_ns;
    bool signaled = false;
    /* As in wait(): an idle task polls */
    const bool may_block = !self->is_idle();

    for (;;)
    {
        if (signaled_.cmpxchg(0, 1) == 1)
        {
            signaled = true;
            break;
        }
        if (get_boot_time().get_value() >= until)
            break;

        if (unlikely(!may_block))
        {
            schedule();
            continue;
        }

        /* wait()'s window, interrupts off from the flag going up until it is
           down again, with the deadline beside it for the scheduler: a
           signal() unblocks the task, and so does its time coming. */
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

    let_go();
    return signaled;
}

void Event::let_go()
{
    /* Emptied first, then the window looked at: a signal() that comes into
       it after this finds no waiter -- it looks at waiter_ again inside --
       and one that was in it before is waited out. Both sides are
       sequentially consistent atomics, so one of the two always sees the
       other. */
    waiter_.set(0);
    if (signallers_.get() == 0)
        return;

    /* A signal() in its window has interrupts off and touches the task for a
       handful of instructions -- the boot's test holds it for longer -- so
       this is a short spin. One that does not end is a signaller wedged with
       its interrupts off, said rather than waited for. */
    let_go_met.inc();
    ulong deadline = get_boot_time().get_value() + LET_GO_STUCK_NS;
    for (ulong spins = 1; signallers_.get() != 0; spins++)
    {
        cpu_relax();
        if (spins % LET_GO_CLOCK_EVERY == 0 && get_boot_time().get_value() >= deadline)
            PANIC("event 0x%p: a signal() still inside its window after %lu s", this,
                  LET_GO_STUCK_NS / consts::NANO_SECS_IN_SEC);
    }
}

ulong Event::let_go_waits()
{
    return (ulong)let_go_met.get();
}

bool Event::poll_for(unsigned long long poll_ns)
{
    /* The waiter's call, as wait()'s is: one at a time */
    BUG_ON(waiter_.get() != 0);

    ulong start = get_boot_time().get_value();
    ulong until = (poll_ns > ~0UL - start) ? ~0UL : start + (ulong)poll_ns;
    for (;;)
    {
        if (signaled_.get() != 0 && signaled_.cmpxchg(0, 1) == 1)
            return true;
        if (get_boot_time().get_value() >= until)
            return false;
        cpu_relax();
    }
}

void Event::signal()
{
    signal_holding(0);
}

void Event::signal_holding(ulong hold_ns)
{
    /* Published first. A plain read before the locked operation, so a burst
       of signals to a busy waiter does not take the event's line from it
       for each.

       But with the flag found up nothing here was a locked instruction, and
       the flag is not all a signal publishes: what the caller made for the
       waiter just before -- a completion pushed onto a lock-free ring, its
       last steps plain stores -- may still be in this CPU's store buffer.
       The waiter that takes the flag now and looks for its work then does
       not see it, finds nothing, and goes back to sleep on a flag this call
       left as it was: work queued, the waiter asleep, and no signal owed.
       On x86 a store followed by a load is the one pair the CPU reorders;
       on arm64 the same holds for a publisher's plain store.

       So a full barrier on that path, and the flag looked at again after
       it. Still up: whoever takes it does so after the work can be seen.
       Taken meanwhile -- perhaps before the work could be seen -- it is put
       up again. Found down the first time, the compare-and-swap is the
       barrier, whether it sets the flag or loses to another signal's. */
    if (signaled_.get() == 0)
    {
        signaled_.cmpxchg(1, 0);
    }
    else
    {
        hal::smp_mb();
        if (signaled_.get() == 0)
            signaled_.cmpxchg(1, 0);
    }

    /* Then the waiter, every time -- not only by whoever set the flag: a
       waiter found blocked with a signal pending is one to wake, whoever's
       signal it was. None in wait() -- a busy waiter's burst -- is a look at
       the event alone. */
    if (waiter_.get() == 0)
        return;

    /* The task touched inside the window only: counted before waiter_ is read
       again, so that the wait() that set it does not return -- and its task
       neither exits nor blocks on anything else -- before this is done with
       it (let_go); interrupts off, so that this is done with it in a moment.
       What the look above saw may have been another wait()'s. */
    ulong flags = hal::irq_save();
    signallers_.inc();

    Task* waiter = (Task*)waiter_.get();
    if (unlikely(hold_ns != 0))
    {
        ulong until = get_boot_time().get_value() + hold_ns;
        while (get_boot_time().get_value() < until)
            cpu_relax();
    }

    bool kick = false;
    ulong cpu = 0;
    if (waiter != nullptr)
    {
        /* Alive by the window: a task gone would be a wait() that let go of
           it with this still in here */
        BUG_ON(waiter->magic_ != Task::TASK_MAGIC);
        if (waiter->is_blocked())
        {
            waiter->unblock();
            cpu = (ulong)waiter_cpu_.get();
            kick = true;
        }
    }

    signallers_.dec();
    hal::irq_restore(flags);

    /* Unblocked is not running: an idle CPU would not look at its queue again
       until the next tick. The IPI ends in schedule(), which finds the task
       runnable -- the same kick SoftIrq::raise_mask gives its tasks. A CPU's
       number, nothing of the task's, which may be gone by now. */
    if (kick)
        hal::send_ipi(cpu, CpuTable::IPI_VECTOR);
}

}
