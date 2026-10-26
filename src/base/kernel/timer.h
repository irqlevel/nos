#pragma once

#include <include/types.h>
#include <lib/stdlib.h>
#include "raw_spin_lock.h"

namespace kernel
{

class TimerCallback
{
public:
    virtual void on_tick(TimerCallback& callback) = 0;
};

class TimerTable final
{
public:
    static TimerTable& get_instance()
    {
        static TimerTable instance;

        return instance;
    }

    bool start_timer(TimerCallback& callback, stdlib::Time period);
    void stop_timer(TimerCallback& callback);

    void process_timers();

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

        TimerCallback *callback;
        TimerCallback *running;   /* callback mid-flight in process_timers */
        ulong running_cpu;         /* CPU executing that callback */
        stdlib::Time period;
        stdlib::Time expired;
    };

    Timer timer_[16];

    /* Protects the timer array. start_timer/stop_timer run in task context on
       any CPU while process_timers runs in IPI context, so all access must be
       under the lock with IRQs disabled. */
    RawSpinLock lock_;
};

}