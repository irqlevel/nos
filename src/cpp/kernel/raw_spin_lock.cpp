#include "raw_spin_lock.h"
#include <hal/cpu.h>
#include "preempt.h"
#include "watchdog.h"

namespace Kernel
{

RawSpinLock::RawSpinLock(bool watched)
    : Watched(watched)
    , WatchdogReported(0)
    , WatchdogLockTime(0)
{
    if (Watched)
        Watchdog::GetInstance().RegisterSpinLock(*this);
}

RawSpinLock::~RawSpinLock()
{
    if (Watched)
        Watchdog::GetInstance().UnregisterSpinLock(*this);
}

/* Raw cycle counter, not GetBootTime(): this runs on every lock in the
   kernel. 0 is the "not held" marker the watchdog reads, so a counter that
   happens to read exactly 0 is nudged rather than losing the timestamp. */
void RawSpinLock::Stamp()
{
    if (!Watched)
        return;

    u64 now = Hal::ReadCycleCounter();
    WatchdogLockTime.Set((now != 0) ? now : 1);
}

void RawSpinLock::Lock()
{
    for (;;)
    {
        if (Value.Cmpxchg(1, 0) == 0)
            break;

        Pause();
    }

    Stamp();
}

bool RawSpinLock::TryLock()
{
    if (Value.Cmpxchg(1, 0) != 0)
        return false;

    Stamp();
    return true;
}

void RawSpinLock::Unlock()
{
    if (Watched)
    {
        WatchdogReported.Set(0);
        WatchdogLockTime.Set(0);
    }
    Value.Set(0);
}

ulong RawSpinLock::LockIrqSave()
{
    ulong flags = PreemptIrqSave();
    Lock();
    return flags;
}

ulong RawSpinLock::TryLockIrqSave(bool& acquired)
{
    ulong flags = PreemptIrqSave();
    acquired = TryLock();
    return flags;
}

void RawSpinLock::UnlockIrqRestore(ulong flags)
{
    Unlock();
    PreemptIrqRestore(flags);
}

}
