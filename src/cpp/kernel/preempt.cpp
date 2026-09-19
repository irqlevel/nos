#include "preempt.h"
#include "task.h"
#include "sched.h"
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
    /* Block until the BSP has globally enabled preemption: PreemptActive
       starts at zero and PreemptOn() sets it to 1. (Atomic's constructors
       are constexpr, so a static one is initialised by the compiler; the
       kernel runs no .init_array, and the link refuses one.) */
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

/* The count has just dropped to zero: a reschedule that came while it was up
   is made now. Only with interrupts on -- off, this is an interrupt handler
   or a section its caller keeps closed, and the reschedule waits for the next
   PreemptEnable, the handler's own Preempt() or the tick. Only for the task
   that is running: a lock can be released on another's behalf, as
   SwitchComplete releases the ones Schedule() took in the task it switched
   away from. And not once a panic has begun, which no tick preempts either. */
static void PreemptRunPending(Task* task)
{
    if (likely(task->PreemptPending.Get() == 0))
        return;

    if (!Hal::IsInterruptEnabled() || task != Task::TryGetCurrentTask() ||
        Panicker::GetInstance().IsActive())
        return;

    Preempt();
}

void PreemptEnable()
{
    if (likely(PreemptIsOn()))
    {
        auto task = Task::GetCurrentTask();
        BugOn(!task);
        BugOn(!task->PreemptDisableCounter.Get());
        if (task->PreemptDisableCounter.DecAndTest())
            PreemptRunPending(task);
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
    if (task->PreemptDisableCounter.DecAndTest())
        PreemptRunPending(task);
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
