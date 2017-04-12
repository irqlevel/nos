#include "preempt.h"
#include "task.h"

namespace Kernel
{

namespace Core
{

volatile bool PreemptActive = false;

void PreemptActivate()
{
    PreemptActive = true;
}

bool PreemptIsActive()
{
    return PreemptActive;
}

void PreemptDisable()
{
    if (likely(PreemptActive))
    {
        Task *task = Task::GetCurrentTask();
        task->PreemptCounter.Inc();
    }
}

void PreemptEnable()
{
    if (likely(PreemptActive))
    {
        Task *task = Task::GetCurrentTask();
        task->PreemptCounter.Dec();
    }
}

}
}
