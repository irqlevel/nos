#include "spin_lock.h"
#include "task.h"
#include <hal/cpu.h>
#include "preempt.h"
#include "watchdog.h"

namespace Kernel
{

SpinLock::SpinLock()
    : Owner(nullptr)
    , WatchdogLockTime(0)
{
    Watchdog::GetInstance().RegisterSpinLock(*this);
}

SpinLock::~SpinLock()
{
    Watchdog::GetInstance().UnregisterSpinLock(*this);
}

void SpinLock::Lock()
{
    RawLock.Lock();
    Owner = (PreemptIsOn()) ? Task::GetCurrentTask() : nullptr;

    /* Raw cycle counter, not GetBootTime(): this runs on every lock in the
       kernel, and until TimeInit() picks the TSC, GetBootTime() reads the
       HPET -- an uncached MMIO access per acquire and per release. The page
       allocator takes ~22 locks per page, which made a self-test page cost
       ~0.2ms on real hardware. Watchdog::Check converts to nanoseconds only
       when it has something to report.

       0 is the "not held" marker below and in Watchdog::Check, so a counter
       that happens to read exactly 0 is nudged rather than losing the
       timestamp. */
    u64 now = Hal::ReadCycleCounter();
    WatchdogLockTime.Set((now != 0) ? now : 1);
}

void SpinLock::Unlock()
{
    WatchdogLockTime.Set(0);
    Owner = nullptr;
    RawLock.Unlock();
}

void SpinLock::Lock(ulong& flags)
{
    flags = PreemptIrqSave();
    Lock();
}

void SpinLock::Unlock(ulong flags)
{
    Unlock();
    PreemptIrqRestore(flags);
}

void SpinLock::SharedLock(ulong& flags)
{
    Lock(flags);
}

void SpinLock::SharedUnlock(ulong flags)
{
    Unlock(flags);
}

}