#include "preempt.h"
#include "task.h"
#include "panic.h"
#include "asm.h"
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

static constexpr ulong PreemptWasOnBit = (1UL << 63);

ulong PreemptIrqSave()
{
    bool preemptOn = PreemptIsOn();
    if (preemptOn)
        PreemptDisable();
    ulong flags = GetRflags();
    if (preemptOn)
        flags |= PreemptWasOnBit;
    InterruptDisable();
    return flags;
}

void PreemptIrqRestore(ulong flags)
{
    bool preemptWasOn = (flags & PreemptWasOnBit) != 0;
    SetRflags(flags & ~PreemptWasOnBit);
    if (preemptWasOn)
        PreemptEnable();
}

}
