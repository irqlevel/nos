#include "raw_spin_lock.h"
#include <hal/cpu.h>
#include "preempt.h"
#include "watchdog.h"

namespace kernel
{

RawSpinLock::RawSpinLock(bool watched)
    : watched_(watched)
    , preempt_task_(nullptr)
    , watchdog_reported_(0)
    , watchdog_lock_time_(0)
{
    if (watched_)
        Watchdog::get_instance().register_spin_lock(*this);
}

RawSpinLock::~RawSpinLock()
{
    if (watched_)
        Watchdog::get_instance().unregister_spin_lock(*this);
}

/* Raw cycle counter, not GetBootTime(): this runs on every lock in the
   kernel. 0 is the "not held" marker the watchdog reads, so a counter that
   happens to read exactly 0 is nudged rather than losing the timestamp. */
void RawSpinLock::stamp()
{
    if (!watched_)
        return;

    u64 now = hal::read_cycle_counter();
    watchdog_lock_time_.set((now != 0) ? now : 1);
}

void RawSpinLock::acquire()
{
    for (;;)
    {
        if (value_.cmpxchg(1, 0) == 0)
            break;

        cpu_relax();
    }

    stamp();
}

bool RawSpinLock::try_acquire()
{
    if (value_.cmpxchg(1, 0) != 0)
        return false;

    stamp();
    return true;
}

void RawSpinLock::release()
{
    if (watched_)
    {
        watchdog_reported_.set(0);
        watchdog_lock_time_.set(0);
    }
    value_.set(0);
}

void RawSpinLock::lock()
{
    /* Before the acquire, not after: a tick landing between the two would
       switch the new holder away with the lock taken. */
    Task* task = preempt_disable_task();
    acquire();
    preempt_task_ = task;
}

void RawSpinLock::unlock()
{
    /* Read and cleared while the lock is still ours: once Release() runs,
       the next holder writes its own. */
    Task* task = preempt_task_;
    preempt_task_ = nullptr;
    release();
    preempt_enable_task(task);
}

bool RawSpinLock::try_lock()
{
    Task* task = preempt_disable_task();
    if (!try_acquire())
    {
        preempt_enable_task(task);
        return false;
    }

    preempt_task_ = task;
    return true;
}

/* The IRQ-saving forms leave PreemptTask alone: PreemptIrqSave disables
   preemption along with interrupts, and the flags carry it to the restore. */
ulong RawSpinLock::lock_irq_save()
{
    ulong flags = preempt_irq_save();
    acquire();
    return flags;
}

ulong RawSpinLock::try_lock_irq_save(bool& acquired)
{
    ulong flags = preempt_irq_save();
    acquired = try_acquire();
    return flags;
}

void RawSpinLock::unlock_irq_restore(ulong flags)
{
    release();
    preempt_irq_restore(flags);
}

}
