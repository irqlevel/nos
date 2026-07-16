#include "raw_rw_spin_lock.h"
#include <hal/cpu.h>
#include "preempt.h"

namespace Kernel
{

RawRwSpinLock::RawRwSpinLock()
{
}

RawRwSpinLock::~RawRwSpinLock()
{
}

void RawRwSpinLock::ReadLock()
{
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
}

void RawRwSpinLock::ReadUnlock()
{
    Value.Dec();
}

void RawRwSpinLock::WriteLock()
{
    WriterWaiting.Inc();
    for (;;)
    {
        if (Value.Cmpxchg(-1, 0) == 0)
            break;

        Pause();
    }
}

void RawRwSpinLock::WriteUnlock()
{
    Value.Set(0);
    WriterWaiting.Dec();
}

ulong RawRwSpinLock::WriteLockIrqSave()
{
    ulong flags = PreemptIrqSave();
    WriteLock();
    return flags;
}

void RawRwSpinLock::WriteUnlockIrqRestore(ulong flags)
{
    WriteUnlock();
    PreemptIrqRestore(flags);
}

}
