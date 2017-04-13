#include "preempt.h"
#include "task.h"
#include "panic.h"
#include "asm.h"

namespace Kernel
{

volatile bool PreemptActive = false;

void PreemptOn()
{
    PreemptActive = true;
    Barrier();
}

void PreemptOff()
{
    PreemptActive = false;
    Barrier();
}

bool PreemptIsOn()
{
    return PreemptActive;
}

void PreemptDisable()
{
    if (likely(PreemptActive))
    {
        auto task = Task::GetCurrentTask();
        task->PreemptCounter.Inc();
    }
}

void PreemptEnable()
{
    if (likely(PreemptActive))
    {
        auto task = Task::GetCurrentTask();
        BugOn(task->PreemptCounter.Get() == 0);
        task->PreemptCounter.Dec();
    }
}

}
