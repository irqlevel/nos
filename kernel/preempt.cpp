#include "preempt.h"
#include "task.h"
#include "panic.h"
#include "asm.h"
#include "debug.h"

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
    Barrier();
    return PreemptActive;
}

void PreemptDisable()
{
    if (likely(PreemptIsOn()))
    {
        auto task = Task::GetCurrentTask();
        task->PreemptDisableCounter.Inc();
    }
}

void PreemptEnable()
{
    if (likely(PreemptIsOn()))
    {
        auto task = Task::GetCurrentTask();
        BugOn(task->PreemptDisableCounter.Get() == 0);
        task->PreemptDisableCounter.Dec();
    }
}

}
