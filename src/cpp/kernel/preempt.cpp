#include "preempt.h"
#include "task.h"
#include "sched.h"
#include "panic.h"
#include <hal/cpu.h>
#include "debug.h"
#include "atomic.h"

namespace kernel
{

Atomic preempt_active;

void preempt_on()
{
    preempt_active.inc();
    BUG_ON(preempt_active.get() > 1);
}

void preempt_on_wait()
{
    /* Block until the BSP has globally enabled preemption: preempt_active
       starts at zero and preempt_on() sets it to 1. (Atomic's constructors
       are constexpr, so a static one is initialised by the compiler; the
       kernel runs no .init_array, and the link refuses one.) */
    while (preempt_active.get() == 0)
    {
        cpu_relax();
    }
}

void preempt_off()
{
    preempt_active.dec();
    BUG_ON(preempt_active.get() != 0);
}

bool preempt_is_on()
{
    return (preempt_active.get() != 0) ? true : false;
}

Task* preempt_current_task()
{
    Task* task = Task::try_get_current_task();
    if (likely(task != nullptr))
        return task;

    if (Panicker::get_instance().is_active())
        return nullptr;

    /* What is wrong with this stack, said by the BUG_ON that finds it */
    return Task::get_current_task();
}

void preempt_disable()
{
    if (likely(preempt_is_on()))
    {
        Task* task = preempt_current_task();
        if (task != nullptr)
            task->preempt_disable_counter_.inc();
    }
}

/* The count has just dropped to zero: a reschedule that came while it was up
   is made now. Only with interrupts on -- off, this is an interrupt handler
   or a section its caller keeps closed, and the reschedule waits for the next
   preempt_enable, the handler's own Preempt() or the tick. Only for the task
   that is running: a lock can be released on another's behalf, as
   switch_complete releases the ones schedule() took in the task it switched
   away from. And not once a panic has begun, which no tick preempts either. */
static void preempt_run_pending(Task* task)
{
    if (likely(task->preempt_pending_.get() == 0))
        return;

    if (!hal::is_interrupt_enabled() || task != Task::try_get_current_task() ||
        Panicker::get_instance().is_active())
        return;

    preempt();
}

void preempt_enable()
{
    if (likely(preempt_is_on()))
    {
        Task* task = preempt_current_task();
        if (task == nullptr)
            return;
        BUG_ON(!task->preempt_disable_counter_.get());
        if (task->preempt_disable_counter_.dec_and_test())
            preempt_run_pending(task);
    }
}

Task* preempt_disable_task()
{
    if (!preempt_is_on())
        return nullptr;

    /* Not get_current_task(), whose BUG_ON is for callers that know they run on
       a task: a lock is taken from stacks that are not one. */
    Task* task = Task::try_get_current_task();
    if (task != nullptr)
        task->preempt_disable_counter_.inc();

    return task;
}

void preempt_enable_task(Task* task)
{
    if (task == nullptr)
        return;

    BUG_ON(task->preempt_disable_counter_.get() == 0);
    if (task->preempt_disable_counter_.dec_and_test())
        preempt_run_pending(task);
}

bool preempt_can_block()
{
    if (!hal::is_interrupt_enabled())
        return false;

    /* Before the scheduler runs nothing is ever switched away, so a wait
       spins through to its end: interrupts are the whole question. */
    if (!preempt_is_on())
        return true;

    Task* task = Task::try_get_current_task();
    return task != nullptr && task->preempt_disable_counter_.get() == 0;
}

static constexpr ulong PREEMPT_WAS_ON_BIT = (1UL << 63);

ulong preempt_irq_save()
{
    bool preempt_on = preempt_is_on();
    if (preempt_on)
        preempt_disable();
    ulong flags = hal::irq_save();
    if (preempt_on)
        flags |= PREEMPT_WAS_ON_BIT;
    return flags;
}

void preempt_irq_restore(ulong flags)
{
    bool preempt_was_on = (flags & PREEMPT_WAS_ON_BIT) != 0;
    hal::irq_restore(flags & ~PREEMPT_WAS_ON_BIT);
    if (preempt_was_on)
        preempt_enable();
}

}
