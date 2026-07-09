#include "timer.h"
#include "time.h"
#include "cpu.h"

namespace Kernel
{

TimerTable::Timer::Timer()
    : Callback(nullptr)
    , Running(nullptr)
    , RunningCpu(0)
{
}

TimerTable::TimerTable()
{
}

TimerTable::~TimerTable()
{
}

bool TimerTable::StartTimer(TimerCallback& callback, Stdlib::Time period)
{
    /* A zero period would re-fire the callback on every tick */
    if (period.GetValue() == 0)
        return false;

    ulong flags = Lock.LockIrqSave();
    for (size_t i = 0; i < Stdlib::ArraySize(Timer); i++)
    {
        auto& timer = Timer[i];
        if (timer.Callback == nullptr)
        {
            timer.Period = period;
            timer.Expired = GetBootTime() + period;
            timer.Callback = &callback;
            Lock.UnlockIrqRestore(flags);
            return true;
        }
    }
    Lock.UnlockIrqRestore(flags);

    return false;
}

void TimerTable::StopTimer(TimerCallback& callback)
{
    ulong flags = Lock.LockIrqSave();
    for (size_t i = 0; i < Stdlib::ArraySize(Timer); i++)
    {
        auto& timer = Timer[i];
        if (timer.Callback == &callback)
        {
            timer.Callback = nullptr;
        }
    }
    Lock.UnlockIrqRestore(flags);

    /* Do not return while the callback is mid-flight in ProcessTimers on
       another CPU: the caller is allowed to free the callback right after
       StopTimer. If it is running on *this* CPU we are inside OnTick itself,
       where waiting would self-deadlock and is unnecessary (the callback is
       still on our own stack). */
    for (size_t i = 0; i < Stdlib::ArraySize(Timer); i++)
    {
        for (;;)
        {
            flags = Lock.LockIrqSave();
            bool busy = (Timer[i].Running == &callback) &&
                (Timer[i].RunningCpu != CpuTable::GetInstance().GetCurrentCpuId());
            Lock.UnlockIrqRestore(flags);
            if (!busy)
                break;
            Pause();
        }
    }
}

void TimerTable::ProcessTimers()
{
    auto now = GetBootTime();

    for (size_t i = 0; i < Stdlib::ArraySize(Timer); i++)
    {
        auto& timer = Timer[i];

        ulong flags = Lock.LockIrqSave();
        TimerCallback* callback = timer.Callback;
        if (callback == nullptr || now < timer.Expired)
        {
            Lock.UnlockIrqRestore(flags);
            continue;
        }

        /* Skip missed ticks: += Period after a long delay would fire
           the callback on every tick until it catches up */
        timer.Expired = now + timer.Period;
        timer.Running = callback;
        timer.RunningCpu = CpuTable::GetInstance().GetCurrentCpuId();
        Lock.UnlockIrqRestore(flags);

        /* Invoke outside the lock: OnTick may call Start/StopTimer */
        callback->OnTick(*callback);

        flags = Lock.LockIrqSave();
        timer.Running = nullptr;
        Lock.UnlockIrqRestore(flags);
    }
}

}
