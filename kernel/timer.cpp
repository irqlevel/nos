#include "timer.h"
#include "time.h"

namespace Kernel
{

TimerTable::Timer::Timer()
    : Callback(nullptr)
{
}

TimerTable::TimerTable()
{
}

TimerTable::~TimerTable()
{
}

bool TimerTable::StartTimer(TimerCallback& callback, Shared::Time period)
{
    for (size_t i = 0; i < Shared::ArraySize(Timer); i++)
    {
        auto& timer = Timer[i];
        if (timer.Callback == nullptr)
        {
            timer.Period = period;
            timer.Expired = GetBootTime() + period;
            timer.Callback = &callback;
            return true;
        }
    }

    return false;
}

void TimerTable::StopTimer(TimerCallback& callback)
{
    for (size_t i = 0; i < Shared::ArraySize(Timer); i++)
    {
        auto& timer = Timer[i];
        if (timer.Callback == &callback)
        {
            timer.Callback = nullptr;
        }
    }
}

void TimerTable::ProcessTimers()
{
    auto now = GetBootTime();

    for (size_t i = 0; i < Shared::ArraySize(Timer); i++)
    {
        auto& timer = Timer[i];

        if (timer.Callback != nullptr && now >= timer.Expired)
        {
            timer.Callback->OnTick(*timer.Callback);

            timer.Expired += timer.Period;
        }
    }
}

}