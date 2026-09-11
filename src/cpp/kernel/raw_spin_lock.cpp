#include "raw_spin_lock.h"
#include <hal/cpu.h>
#include "preempt.h"
#include "watchdog.h"

namespace Kernel
{

RawSpinLock::RawSpinLock(bool watched)
    : Watched(watched)
    , PreemptTask(nullptr)
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

void RawSpinLock::Acquire()
{
    for (;;)
    {
        if (Value.Cmpxchg(1, 0) == 0)
            break;

        Pause();
    }

    Stamp();
}

bool RawSpinLock::TryAcquire()
{
    if (Value.Cmpxchg(1, 0) != 0)
        return false;

    Stamp();
    return true;
}

void RawSpinLock::Release()
{
    if (Watched)
    {
        WatchdogReported.Set(0);
        WatchdogLockTime.Set(0);
    }
    Value.Set(0);
}

void RawSpinLock::Lock()
{
    /* Before the acquire, not after: a tick landing between the two would
       switch the new holder away with the lock taken. */
    Task* task = PreemptDisableTask();
    Acquire();
    PreemptTask = task;
}

void RawSpinLock::Unlock()
{
    /* Read and cleared while the lock is still ours: once Release() runs,
       the next holder writes its own. */
    Task* task = PreemptTask;
    PreemptTask = nullptr;
    Release();
    PreemptEnableTask(task);
}

bool RawSpinLock::TryLock()
{
    Task* task = PreemptDisableTask();
    if (!TryAcquire())
    {
        PreemptEnableTask(task);
        return false;
    }

    PreemptTask = task;
    return true;
}

/* The IRQ-saving forms leave PreemptTask alone: PreemptIrqSave disables
   preemption along with interrupts, and the flags carry it to the restore. */
ulong RawSpinLock::LockIrqSave()
{
    ulong flags = PreemptIrqSave();
    Acquire();
    return flags;
}

ulong RawSpinLock::TryLockIrqSave(bool& acquired)
{
    ulong flags = PreemptIrqSave();
    acquired = TryAcquire();
    return flags;
}

void RawSpinLock::UnlockIrqRestore(ulong flags)
{
    Release();
    PreemptIrqRestore(flags);
}

}
