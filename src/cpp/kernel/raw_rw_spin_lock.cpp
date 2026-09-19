#include "raw_rw_spin_lock.h"
#include <hal/cpu.h>
#include "preempt.h"

namespace Kernel
{

Task* RawRwSpinLock::ReadLock()
{
    /* Before the acquire, as in RawSpinLock::Lock(): a tick landing between
       the two would switch a reader away with the lock held. */
    Task* task = PreemptDisableTask();

    for (;;)
    {
        if (WriterWaiting.Get() != 0)
        {
            Pause();
            continue;
        }

        long v = Value.Get();
        if (v >= 0 && Value.Cmpxchg(v + 1, v) == v)
        {
            /* A writer may have started waiting between the check above
               and the cmpxchg; back out so it isn't starved */
            if (WriterWaiting.Get() != 0)
            {
                Value.Dec();
                Pause();
                continue;
            }
            break;
        }

        Pause();
    }

    return task;
}

void RawRwSpinLock::ReadUnlock(Task* preemptTask)
{
    Value.Dec();
    PreemptEnableTask(preemptTask);
}

void RawRwSpinLock::AcquireWrite()
{
    WriterWaiting.Inc();
    for (;;)
    {
        if (Value.Cmpxchg(-1, 0) == 0)
            break;

        Pause();
    }
}

void RawRwSpinLock::ReleaseWrite()
{
    Value.Set(0);
    WriterWaiting.Dec();
}

void RawRwSpinLock::WriteLock()
{
    Task* task = PreemptDisableTask();
    AcquireWrite();
    WriterPreemptTask = task;
}

void RawRwSpinLock::WriteUnlock()
{
    /* Read and cleared while the lock is still ours: once it is released,
       the next writer records its own. */
    Task* task = WriterPreemptTask;
    WriterPreemptTask = nullptr;
    ReleaseWrite();
    PreemptEnableTask(task);
}

ulong RawRwSpinLock::WriteLockIrqSave()
{
    ulong flags = PreemptIrqSave();
    AcquireWrite();
    return flags;
}

void RawRwSpinLock::WriteUnlockIrqRestore(ulong flags)
{
    ReleaseWrite();
    PreemptIrqRestore(flags);
}

}
