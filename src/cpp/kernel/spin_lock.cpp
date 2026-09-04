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