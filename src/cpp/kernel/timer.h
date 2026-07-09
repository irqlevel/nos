#pragma once

#include <include/types.h>
#include <lib/stdlib.h>
#include "raw_spin_lock.h"

namespace Kernel
{

class TimerCallback
{
public:
    virtual void OnTick(TimerCallback& callback) = 0;
};

class TimerTable final
{
public:
    static TimerTable& GetInstance()
    {
        static TimerTable Instance;

        return Instance;
    }

    bool StartTimer(TimerCallback& callback, Stdlib::Time period);
    void StopTimer(TimerCallback& callback);

    void ProcessTimers();

private:
    TimerTable();
    ~TimerTable();

    TimerTable(const TimerTable& other) = delete;
    TimerTable(TimerTable&& other) = delete;
    TimerTable& operator=(const TimerTable& other) = delete;
    TimerTable& operator=(TimerTable&& other) = delete;

    struct Timer
    {
        Timer();

        TimerCallback *Callback;
        TimerCallback *Running;   /* callback mid-flight in ProcessTimers */
        ulong RunningCpu;         /* CPU executing that callback */
        Stdlib::Time Period;
        Stdlib::Time Expired;
    };

    Timer Timer[16];

    /* Protects the timer array. StartTimer/StopTimer run in task context on
       any CPU while ProcessTimers runs in IPI context, so all access must be
       under the lock with IRQs disabled. */
    RawSpinLock Lock;
};

}