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
    WatchdogLockTime.Set(GetBootTime().GetValue());
}

void SpinLock::Unlock()
{
    Stdlib::Time lockTime(WatchdogLockTime.Get());
    if (lockTime.GetValue() != 0)
    {
        Stdlib::Time now = GetBootTime();
        Stdlib::Time delta = now - lockTime;
        if (delta > Stdlib::Time(20 * Const::NanoSecsInMs))
        {
            //Panic("Spin lock 0x%p was held too long %u", this, delta.GetValue());
        }
    }

    WatchdogLockTime.Set(0);
    Owner = nullptr;
    RawLock.Unlock();
}

/*
 * Bit 63 of saved rflags is always zero in hardware.
 * We borrow it to record whether PreemptDisable() was
 * actually called, so Unlock uses the same decision.
 * This closes a race where PreemptIsOn() changes between
 * Lock and Unlock (e.g. PreemptOn() called by another CPU).
 */
static constexpr ulong PreemptWasOnBit = (1UL << 63);

void SpinLock::Lock(ulong& flags)
{
    bool preemptOn = PreemptIsOn();
    if (preemptOn)
        PreemptDisable();
    flags = GetRflags();
    if (preemptOn)
        flags |= PreemptWasOnBit;
    InterruptDisable();
    Lock();
}

void SpinLock::Unlock(ulong flags)
{
    bool preemptWasOn = (flags & PreemptWasOnBit) != 0;
    Unlock();
    SetRflags(flags & ~PreemptWasOnBit);
    if (preemptWasOn)
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