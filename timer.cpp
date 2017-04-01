#include "timer.h"
#include "pit.h"

namespace Kernel
{

namespace Core
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
            timer.Expired = Pit::GetInstance().GetTime();
            timer.Expired.Add(period);
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
    auto now = Pit::GetInstance().GetTime();

    for (size_t i = 0; i < Shared::ArraySize(Timer); i++)
    {
        auto& timer = Timer[i];

        if (timer.Callback != nullptr && now.Compare(timer.Expired) > 0)
        {
            timer.Callback->OnTick(*timer.Callback);

            auto expired = now;
            expired.Add(timer.Period);
            timer.Expired = expired;
        }
    }
}

}
}