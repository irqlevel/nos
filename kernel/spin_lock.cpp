#include "spin_lock.h"
#include "task.h"
#include "asm.h"
#include "preempt.h"
#include "time.h"
#include "watchdog.h"

namespace Kernel
{

SpinLock::SpinLock()
    : RawLock(0)
    , Owner(nullptr)
    , LockTime(0)
{
    Watchdog::GetInstance().RegisterSpinLock(*this);
}

SpinLock::~SpinLock()
{
    Watchdog::GetInstance().UnregisterSpinLock(*this);
}

void SpinLock::Lock()
{
    void* owner = (PreemptIsOn()) ? Task::GetCurrentTask() : nullptr;
    ulong attempts = 0;

    for (;;)
    {
        if (RawLock.Cmpxchg(1, 0) == 0)
            break;

        BugOn(owner != nullptr && owner == Owner);
        Pause();
        attempts++;
        if (attempts >= 1000000)
            Panic("Can't acquire spin lock 0x%p", this);
    }

    Owner = (PreemptIsOn()) ? Task::GetCurrentTask() : nullptr;
    LockTime.Set(GetBootTime().GetValue());
}

void SpinLock::Unlock()
{
    LockTime.Set(0);
    Owner = nullptr;
    RawLock.Set(0);
}

void SpinLock::Lock(ulong& flags)
{
    flags = GetRflags();
    InterruptDisable();
    PreemptDisable();
    Lock();
}

void SpinLock::Unlock(ulong flags)
{
    Unlock();
    SetRflags(flags);
    PreemptEnable();
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