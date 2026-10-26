#include "timer.h"
#include "time.h"
#include "cpu.h"

namespace kernel
{

TimerTable::Timer::Timer()
    : callback(nullptr)
    , running(nullptr)
    , running_cpu(0)
{
}

TimerTable::TimerTable()
{
}

TimerTable::~TimerTable()
{
}

bool TimerTable::start_timer(TimerCallback& callback, stdlib::Time period)
{
    /* A zero period would re-fire the callback on every tick */
    if (period.get_value() == 0)
        return false;

    ulong flags = lock_.lock_irq_save();
    for (size_t i = 0; i < stdlib::array_size(timer_); i++)
    {
        auto& timer = timer_[i];
        if (timer.callback == nullptr)
        {
            timer.period = period;
            timer.expired = get_boot_time() + period;
            timer.callback = &callback;
            lock_.unlock_irq_restore(flags);
            return true;
        }
    }
    lock_.unlock_irq_restore(flags);

    return false;
}

void TimerTable::stop_timer(TimerCallback& callback)
{
    ulong flags = lock_.lock_irq_save();
    for (size_t i = 0; i < stdlib::array_size(timer_); i++)
    {
        auto& timer = timer_[i];
        if (timer.callback == &callback)
        {
            timer.callback = nullptr;
        }
    }
    lock_.unlock_irq_restore(flags);

    /* Do not return while the callback is mid-flight in ProcessTimers on
       another CPU: the caller is allowed to free the callback right after
       StopTimer. If it is running on *this* CPU we are inside OnTick itself,
       where waiting would self-deadlock and is unnecessary (the callback is
       still on our own stack). */
    for (size_t i = 0; i < stdlib::array_size(timer_); i++)
    {
        for (;;)
        {
            flags = lock_.lock_irq_save();
            bool busy = (timer_[i].running == &callback) &&
                (timer_[i].running_cpu != CpuTable::get_instance().get_current_cpu_id());
            lock_.unlock_irq_restore(flags);
            if (!busy)
                break;
            cpu_relax();
        }
    }
}

void TimerTable::process_timers()
{
    auto now = get_boot_time();

    for (size_t i = 0; i < stdlib::array_size(timer_); i++)
    {
        auto& timer = timer_[i];

        ulong flags = lock_.lock_irq_save();
        TimerCallback* callback = timer.callback;
        if (callback == nullptr)
        {
            lock_.unlock_irq_restore(flags);
            continue;
        }

        if (now < timer.expired)
        {
            /* A deadline further out than one period cannot have been set
               by this timer: the clock moved backwards underneath it (a
               clock source switch, a migrated VM). Re-anchor it rather
               than let the callback go quiet until the new clock catches
               up with the old one's reading. */
            if (timer.expired > now + timer.period)
                timer.expired = now + timer.period;

            lock_.unlock_irq_restore(flags);
            continue;
        }

        /* Skip missed ticks: += Period after a long delay would fire
           the callback on every tick until it catches up */
        timer.expired = now + timer.period;
        timer.running = callback;
        timer.running_cpu = CpuTable::get_instance().get_current_cpu_id();
        lock_.unlock_irq_restore(flags);

        /* Invoke outside the lock: OnTick may call Start/StopTimer */
        callback->on_tick(*callback);

        flags = lock_.lock_irq_save();
        timer.running = nullptr;
        lock_.unlock_irq_restore(flags);
    }
}

}
