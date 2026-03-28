#include "raw_spin_lock.h"
#include "asm.h"
#include "preempt.h"

namespace Kernel
{

RawSpinLock::RawSpinLock()
{
}
RawSpinLock::~RawSpinLock()
{
}

void RawSpinLock::Lock()
{
    for (;;)
    {
        if (Value.Cmpxchg(1, 0) == 0)
            break;

        Pause();
    }    
}

void RawSpinLock::Unlock()
{
    Value.Set(0);
}

ulong RawSpinLock::LockIrqSave()
{
    ulong flags = PreemptIrqSave();
    Lock();
    return flags;
}

void RawSpinLock::UnlockIrqRestore(ulong flags)
{
    Unlock();
    PreemptIrqRestore(flags);
}

}
