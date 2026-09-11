#include "spin_lock.h"
#include "task.h"
#include <hal/cpu.h>
#include "preempt.h"
#include "watchdog.h"

namespace Kernel
{

SpinLock::SpinLock()
    : Owner(nullptr)
{
}

SpinLock::~SpinLock()
{
}

void SpinLock::Unwatch()
{
    Watchdog::GetInstance().UnregisterSpinLock(RawLock);
}

void SpinLock::Lock()
{
    RawLock.Lock();
    Owner = (PreemptIsOn()) ? Task::GetCurrentTask() : nullptr;
}

void SpinLock::Unlock()
{
    Owner = nullptr;
    RawLock.Unlock();
}

/* Through the raw lock's IRQ-saving form: preemption goes off with
   interrupts and the flags carry it, instead of Lock() counting it a second
   time. */
void SpinLock::Lock(ulong& flags)
{
    flags = RawLock.LockIrqSave();
    Owner = (PreemptIsOn()) ? Task::GetCurrentTask() : nullptr;
}

void SpinLock::Unlock(ulong flags)
{
    Owner = nullptr;
    RawLock.UnlockIrqRestore(flags);
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