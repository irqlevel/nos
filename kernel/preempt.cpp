#include "preempt.h"
#include "task.h"
#include "panic.h"
#include "asm.h"
#include "debug.h"
#include "atomic.h"

namespace Kernel
{

Atomic PreemptActive;
Atomic PreemptOnWaiting(1);

void PreemptOn()
{
    PreemptActive.Inc();
    BugOn(PreemptActive.Get() > 1);
    PreemptOnWaiting.Dec();
}

void PreemptOnWait()
{
    while (PreemptOnWaiting.Get() != 0)
    {
        Pause();
    }
}

void PreemptOff()
{
    PreemptActive.Dec();
    BugOn(PreemptActive.Get() != 0);
}

bool PreemptIsOn()
{
    return (PreemptActive.Get() != 0) ? true : false;
}

void PreemptDisable()
{
    if (likely(PreemptIsOn()))
    {
        auto task = Task::GetCurrentTask();
        if (task)
            task->PreemptDisableCounter.Inc();
    }
}

void PreemptEnable()
{
    if (likely(PreemptIsOn()))
    {
        auto task = Task::GetCurrentTask();
        if (task && task->PreemptDisableCounter.Get() != 0)
            task->PreemptDisableCounter.Dec();
    }
}

}
