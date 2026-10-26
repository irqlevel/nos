#include "raw_rw_spin_lock.h"
#include <hal/cpu.h>
#include "preempt.h"

namespace kernel
{

Task* RawRwSpinLock::read_lock()
{
    /* Before the acquire, as in RawSpinLock::lock(): a tick landing between
       the two would switch a reader away with the lock held. */
    Task* task = preempt_disable_task();

    for (;;)
    {
        if (writer_waiting_.get() != 0)
        {
            cpu_relax();
            continue;
        }

        long v = value_.get();
        if (v >= 0 && value_.cmpxchg(v + 1, v) == v)
        {
            /* A writer may have started waiting between the check above
               and the cmpxchg; back out so it isn't starved */
            if (writer_waiting_.get() != 0)
            {
                value_.dec();
                cpu_relax();
                continue;
            }
            break;
        }

        cpu_relax();
    }

    return task;
}

void RawRwSpinLock::read_unlock(Task* preempt_task)
{
    value_.dec();
    preempt_enable_task(preempt_task);
}

void RawRwSpinLock::acquire_write()
{
    writer_waiting_.inc();
    for (;;)
    {
        if (value_.cmpxchg(-1, 0) == 0)
            break;

        cpu_relax();
    }
}

void RawRwSpinLock::release_write()
{
    value_.set(0);
    writer_waiting_.dec();
}

void RawRwSpinLock::write_lock()
{
    Task* task = preempt_disable_task();
    acquire_write();
    writer_preempt_task_ = task;
}

void RawRwSpinLock::write_unlock()
{
    /* Read and cleared while the lock is still ours: once it is released,
       the next writer records its own. */
    Task* task = writer_preempt_task_;
    writer_preempt_task_ = nullptr;
    release_write();
    preempt_enable_task(task);
}

ulong RawRwSpinLock::write_lock_irq_save()
{
    ulong flags = preempt_irq_save();
    acquire_write();
    return flags;
}

void RawRwSpinLock::write_unlock_irq_restore(ulong flags)
{
    release_write();
    preempt_irq_restore(flags);
}

}
