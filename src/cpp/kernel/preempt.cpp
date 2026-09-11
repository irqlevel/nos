#include "preempt.h"
#include "task.h"
#include "panic.h"
#include <hal/cpu.h>
#include "debug.h"
#include "atomic.h"

namespace Kernel
{

Atomic PreemptActive;

void PreemptOn()
{
    PreemptActive.Inc();
    BugOn(PreemptActive.Get() > 1);
}

void PreemptOnWait()
{
    /* Block until the BSP has globally enabled preemption. PreemptActive is
       zero-initialized (BSS) and set to 1 by PreemptOn(); gating on it rather
       than on a separately-constructed flag avoids depending on a global
       constructor -- this kernel runs no .init_array, so a dynamically
       initialized `Atomic x(1)` would in fact be left at 0. */
    while (PreemptActive.Get() == 0)
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
        BugOn(!task);
        task->PreemptDisableCounter.Inc();
    }
}

void PreemptEnable()
{
    if (likely(PreemptIsOn()))
    {
        auto task = Task::GetCurrentTask();
        BugOn(!task);
        BugOn(!task->PreemptDisableCounter.Get());
        task->PreemptDisableCounter.Dec();
    }
}

Task* PreemptDisableTask()
{
    if (!PreemptIsOn())
        return nullptr;

    /* Not GetCurrentTask(), whose BugOn is for callers that know they run on
       a task: a lock is taken from stacks that are not one. */
    Task* task = Task::TryGetCurrentTask();
    if (task != nullptr)
        task->PreemptDisableCounter.Inc();

    return task;
}

void PreemptEnableTask(Task* task)
{
    if (task == nullptr)
        return;

    BugOn(task->PreemptDisableCounter.Get() == 0);
    task->PreemptDisableCounter.Dec();
}

bool PreemptCanBlock()
{
    if (!Hal::IsInterruptEnabled())
        return false;

    /* Before the scheduler runs nothing is ever switched away, so a wait
       spins through to its end: interrupts are the whole question. */
    if (!PreemptIsOn())
        return true;

    Task* task = Task::TryGetCurrentTask();
    return task != nullptr && task->PreemptDisableCounter.Get() == 0;
}

static constexpr ulong PreemptWasOnBit = (1UL << 63);

ulong PreemptIrqSave()
{
    bool preemptOn = PreemptIsOn();
    if (preemptOn)
        PreemptDisable();
    ulong flags = Hal::IrqSave();
    if (preemptOn)
        flags |= PreemptWasOnBit;
    return flags;
}

void PreemptIrqRestore(ulong flags)
{
    bool preemptWasOn = (flags & PreemptWasOnBit) != 0;
    Hal::IrqRestore(flags & ~PreemptWasOnBit);
    if (preemptWasOn)
        PreemptEnable();
}

}
