#pragma once

#include <include/types.h>
#include <lib/stdlib.h>

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
        Stdlib::Time Period;
        Stdlib::Time Expired;
    };

    Timer Timer[16];
};

}