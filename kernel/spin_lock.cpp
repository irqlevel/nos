#include "spin_lock.h"
#include "task.h"
#include "asm.h"
#include "preempt.h"
#include "time.h"
#include "watchdog.h"

namespace Kernel
{

SpinLock::SpinLock()
    : Owner(nullptr)
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
    RawLock.Lock();
    Owner = (PreemptIsOn()) ? Task::GetCurrentTask() : nullptr;
    LockTime.Set(GetBootTime().GetValue());
}

void SpinLock::Unlock()
{
    Shared::Time lockTime(LockTime.Get());
    if (lockTime.GetValue() != 0)
    {
        Shared::Time now = GetBootTime();
        Shared::Time delta = now - lockTime;
        if (delta > Shared::Time(20 * Shared::NanoSecsInMs))
        {
            //Panic("Spin lock 0x%p was held too long %u", this, delta.GetValue());
        }
    }

    LockTime.Set(0);
    Owner = nullptr;
    RawLock.Unlock();
}

void SpinLock::Lock(ulong& flags)
{
    PreemptDisable();
    flags = GetRflags();
    InterruptDisable();
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