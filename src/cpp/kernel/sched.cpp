#include "sched.h"
#include "panic.h"
#include "trace.h"
#include "preempt.h"
#include "time.h"
#include <hal/cpu.h>
#include "debug.h"
#include "cpu.h"

namespace Kernel
{

/* Tasks moved from one CPU's queue to another. This used to happen on every
   context switch by construction; it is now supposed to be rare, and a
   number is the only way to know whether it is. */
static Atomic TaskMigrations;

/* Reschedules that found the task they landed on with preemption off, and
   of those, how many landed on an idle task. */
static Atomic PreemptsDeferred;
static Atomic PreemptsDeferredIdle;

long GetTaskMigrationCount()
{
    return TaskMigrations.Get();
}

long GetPreemptDeferredCount()
{
    return PreemptsDeferred.Get();
}

long GetPreemptDeferredIdleCount()
{
    return PreemptsDeferredIdle.Get();
}

TaskQueue::TaskQueue()
{
    Stdlib::AutoLock lock(Lock);
    TaskList.Init();
    ExitedList.Init();

    SwitchContextCounter.Set(0);
    ScheduleCounter.Set(0);
    TaskCount.Set(0);
}

void TaskQueue::ReapExited()
{
    for (;;)
    {
        Task* task = nullptr;
        {
            Stdlib::AutoLock lock(ExitedLock);
            if (ExitedList.IsEmpty())
                break;
            Stdlib::ListEntry* entry = ExitedList.RemoveHead();
            task = CONTAINING_RECORD(entry, Task, ListEntry);
        }
        /* Put() here (interrupts enabled) may free the 32KB stack via a
           blocking cross-CPU TLB shootdown; that is safe now but was not in
           the IRQs-off SwitchComplete path where the task was enqueued. */
        task->Put();
    }
}

void TaskQueue::SwitchComplete(Task* curr)
{
    Task* prev = curr->Prev;

    curr->Prev = nullptr;
    curr->Lock.Unlock();
    prev->Lock.Unlock();
    Lock.Unlock();

    if (prev->State.Get() != Task::StateExited)
    {
        auto taskQueue = prev->SelectNextTaskQueue();
        if (taskQueue != nullptr)
        {
            TaskMigrations.Inc();
            prev->Get();
            prev->TaskQueue->Remove(prev);
            taskQueue->Insert(prev);
            prev->Put();
        }

        /* Schedule()'s count should be the only one left: the locks
           TaskQueue::Schedule took in prev were released above, in this
           task, and handed their counts back to prev -- a RawSpinLock
           remembers whose preemption it disabled. Anything more would be a
           lock prev still holds, which Schedule() never switches away from,
           or a count gone astray; either way SelectNext would pass prev over
           for good, a task that silently never runs again. Checked before
           the Dec, while that last count still keeps prev off every CPU:
           after it, prev may already be running elsewhere -- it may just
           have been moved -- and taking locks of its own. */
        BugOn(prev->PreemptDisableCounter.Get() != 1);
        prev->PreemptDisableCounter.Dec();
    } else {

        prev->PreemptDisableCounter.Dec();
        /* Defer the final Put() -- which frees the 32KB stack via a blocking
           cross-CPU TLB shootdown -- out of this IRQs-disabled context. Two
           CPUs freeing exited stacks here concurrently could deadlock on each
           other's shootdown IPI. Enqueue the task and let ReapExited() free it
           from the idle loop with interrupts enabled. prev->ListEntry was
           RemoveInit'd in Schedule, so it is free to reuse. */
        Stdlib::AutoLock lock(ExitedLock);
        ExitedList.InsertTail(&prev->ListEntry);
    }
}

void TaskQueue::SwitchComplete(void* ctx)
{
    Task* curr = static_cast<Task*>(ctx);
    curr->TaskQueue->SwitchComplete(curr);
}

void TaskQueue::Switch(Task* next, Task* curr)
{
    SwitchContextCounter.Inc();

    BugOn(curr == next);
    BugOn(next->Prev != nullptr);
    BugOn(next->Rsp == 0);

    if (curr->State.Get() != Task::StateExited)
        curr->State.Set(Task::StateWaiting);

    curr->ContextSwitches.Inc();
    curr->UpdateRuntime();

    BugOn(next->State.Get() == Task::StateExited);
    next->State.Set(Task::StateRunning);
    next->RunStartTime = GetBootTime();
    next->Prev = curr;
    SwitchContext(next->Rsp, &curr->Rsp, &TaskQueue::SwitchComplete, next);
}

/* Whether cand, a blocked task, is one asleep in Sleep() whose time has
   come. The clock is read at most once a walk, and only by a walk that meets
   such a task: *now is 0 until then. */
static bool SleepIsOver(Task* cand, ulong* now)
{
    ulong until = (ulong)cand->SleepUntil.Get();
    if (until == 0)
        return false;

    if (*now == 0)
        *now = GetBootTime().GetValue();

    return *now >= until;
}

Task* TaskQueue::SelectNext(Task *curr, bool keepOverIdle)
{
    Task* next = nullptr;
    Task* idle = nullptr;
    ulong now = 0;

    for (auto currEntry = TaskList.Flink;
        currEntry != &TaskList;
        currEntry = currEntry->Flink)
    {
        Task* cand = CONTAINING_RECORD(currEntry, Task, ListEntry);
        if (cand == curr)
        {
            BugOn(cand->State.Get() == Task::StateExited);
            cand->ListEntry.Remove();
            TaskList.InsertTail(&cand->ListEntry);
            break;
        }

        if (cand->PreemptDisableCounter.Get() != 0)
        {
            continue;
        }

        /* Asked to be left alone until woken. Skipped outright, and not
           kept as a fallback the way idle is: a blocked task has nothing to
           run. The flag is cleared by whoever has work for it, which is what
           brings it back into this walk -- except for a task asleep in
           Sleep(), which nobody wakes: this walk runs it once its time has
           come, and it clears the flag itself. */
        if (cand->IsBlocked() && !SleepIsOver(cand, &now))
        {
            continue;
        }

        /* The idle task takes its turn only when there is no other turn to
           take. It used to sit in this list as an equal, so a reschedule --
           and every raise of a softirq from an interrupt handler forces one,
           by way of the IPI -- could pick it over the task that had just
           been given work. It then halted the CPU, and the work waited for
           whatever interrupt came next. */
        if (cand->IsIdle())
        {
            if (idle == nullptr)
                idle = cand;
            continue;
        }

        next = cand;
        break;
    }

    /* Only the idle task to hand the CPU to: a task polling with
       YieldToRunnable, or one an interrupt landed on (Preempt), keeps it
       instead -- unless it cannot run itself */
    if (next == nullptr && keepOverIdle &&
        curr->State.Get() != Task::StateExited && !curr->IsBlocked())
        return nullptr;

    return (next != nullptr) ? next : idle;
}

void TaskQueue::Schedule(Task* curr, bool keepOverIdle)
{
    ScheduleCounter.Inc();

    ulong flags = Hal::IrqSave();

    /* Whatever reschedule was put off until now, this is it */
    curr->PreemptPending.Set(0);
    Lock.Lock();

    Task* next = nullptr;
    ulong exitedRetries = 0;
    for (;;) {
        curr->Lock.Lock();
        BugOn(TaskList.IsEmpty());

        if (curr->State.Get() == Task::StateExited && curr->TaskQueue != nullptr)
        {
            BugOn(curr->TaskQueue != this);
            BugOn(curr->ListEntry.IsEmpty());
            curr->TaskQueue = nullptr;
            curr->ListEntry.RemoveInit();
            TaskCount.Dec();
            BugOn(TaskCount.Get() < 0);
        }

        next = SelectNext(curr, keepOverIdle);
        if (next != nullptr)
        {
            next->Lock.Lock();
            next->ListEntry.Remove();
            if (curr->TaskQueue != nullptr)
            {
                BugOn(curr->TaskQueue != this);
                curr->ListEntry.Remove();
                TaskList.InsertTail(&curr->ListEntry);
            }
            TaskList.InsertTail(&next->ListEntry);
            break;
        }

        if (curr->State.Get() != Task::StateExited)
        {
            break;
        }

        /* An exited task can't be returned to: drop the locks and wait
           for a runnable candidate (the idle task at the latest) */
        curr->Lock.Unlock();
        Lock.Unlock();
        if (++exitedRetries > MaxExitedRetries)
            Panic("Schedule: exited task but no runnable candidate");
        Pause();
        Lock.Lock();
    }

    if (next == nullptr)
    {
        curr->UpdateRuntime();
        curr->Lock.Unlock();
        Lock.Unlock();

        /* The count down before interrupts come back on, not after: one
           waiting to be taken -- the IPI for a task woken meanwhile -- would
           find it still up, and its reschedule would wait for a PreemptEnable
           that a task which has just kept the CPU may be a long way from. */
        curr->PreemptDisableCounter.Dec();
        Hal::IrqRestore(flags);
        BugOn(curr->State.Get() == Task::StateExited);
        return;
    }

    Switch(next, curr);
    Hal::IrqRestore(flags);
}

void TaskQueue::Insert(Task* task)
{
    task->Get();

    Stdlib::AutoLock lock(Lock);
    Stdlib::AutoLock lock2(task->Lock);

    BugOn(task->TaskQueue != nullptr);
    BugOn(!(task->ListEntry.IsEmpty()));

    task->TaskQueue = this;
    TaskList.InsertTail(&task->ListEntry);
    TaskCount.Inc();
}

void TaskQueue::Remove(Task* task)
{
    {
        Stdlib::AutoLock lock(Lock);
        Stdlib::AutoLock lock2(task->Lock);

        BugOn(task->TaskQueue != this);
        BugOn(task->ListEntry.IsEmpty());
        task->TaskQueue = nullptr;
        task->ListEntry.RemoveInit();
        TaskCount.Dec();
        BugOn(TaskCount.Get() < 0);
    }

    task->Put();
}

void TaskQueue::Clear()
{
    Stdlib::ListEntry taskList;
    {
        Stdlib::AutoLock lock(Lock);
        taskList.MoveTailList(&TaskList);
        TaskCount.Set(0);
    }

    if (taskList.IsEmpty())
        return;

    while (!taskList.IsEmpty())
    {
        Task* task = CONTAINING_RECORD(taskList.RemoveHead(), Task, ListEntry);
        Stdlib::AutoLock lock2(task->Lock);
        BugOn(task->TaskQueue != this);
        task->TaskQueue = nullptr;
        task->Put();
    }

    Trace(0, "TaskQueue 0x%p counters: sched %u switch context %u",
        this, ScheduleCounter.Get(), SwitchContextCounter.Get());
}

TaskQueue::~TaskQueue()
{
    Clear();
}

long TaskQueue::GetTaskCount()
{
    return TaskCount.Get();
}

long TaskQueue::GetSwitchContextCounter()
{
    return SwitchContextCounter.Get();
}

static void ScheduleCurrent(bool keepOverIdle)
{
    if (unlikely(!PreemptIsOn()))
    {
        return;
    }

    Task *curr = Task::GetCurrentTask();
    if (!curr)
    {
        static long diagOnce = 0;
        if (AtomicReadAndInc(&diagOnce) == 0)
            Task::DiagnoseGetCurrentTask();
        return;
    }

    curr->PreemptDisableCounter.Inc();
    if (curr->PreemptDisableCounter.Get() > 1)
    {
        /* Preemption is off -- a spinlock, most likely -- so not now; but
           remembered, and made the moment the count is back to zero (see
           PreemptEnable). It used to be dropped, and with it whatever the
           reschedule was for: the IPI SoftIrq::Raise sends to run a softirq
           task it has just woken, landing on an idle task inside the lock
           ReapExited takes, left that idle task to halt with the softirq task
           runnable -- and every raise after it found the bit already up and
           sent nothing. The CPU slept on its work until its next tick. */
        curr->PreemptPending.Set(1);
        PreemptsDeferred.Inc();
        if (curr->IsIdle())
            PreemptsDeferredIdle.Inc();

        Stdlib::AutoLock lock(curr->Lock);
        curr->UpdateRuntime();
        curr->PreemptDisableCounter.Dec();
        return;
    }

    curr->TaskQueue->Schedule(curr, keepOverIdle);
}

void Schedule()
{
    ScheduleCurrent(false);
}

void Preempt()
{
    ScheduleCurrent(true);
}

void YieldToRunnable()
{
    ScheduleCurrent(true);
}

/* Sleep blocks. The task is out of the scheduler's walk until its time has
   come, and the walk on its CPU notices that and runs it: at every
   scheduling point that CPU has -- the tick's, 100 times a second, at the
   latest -- which is where a sleeper used to notice it for itself. It used to
   poll, a loop around Schedule() that left it runnable: alone on its CPU it
   let the CPU halt until the tick, but two on one CPU handed it to each other
   without end and the idle task never ran. Two idle hypervisor guests, each
   vCPU asleep until its guest's next timer edge, kept a CPU of the AX41 busy
   that way between them, 46% each. The wakeups come where they came before;
   only the spinning between them is gone.

   Where a task cannot block -- before preemption is on, off a task's stack,
   in a CPU's idle task (which must stay runnable, see Event::Wait), with
   preemption disabled -- it polls as it always did. */
void Sleep(ulong nanoSecs)
{
    ulong start = GetBootTime().GetValue();
    ulong until = (nanoSecs > ~0UL - start) ? ~0UL : start + nanoSecs;

    Task* self = PreemptIsOn() ? Task::TryGetCurrentTask() : nullptr;
    bool block = (self != nullptr && !self->IsIdle() &&
                  self->PreemptDisableCounter.Get() == 0);

    while (GetBootTime().GetValue() < until)
    {
        if (!block)
        {
            Schedule();
            continue;
        }

        /* Interrupts off from the moment the task is marked until it is
           unmarked, as in Event::Wait: a tick landing in between would
           switch it out as a sleeper -- harmless, it is one -- but then
           return it here to go through the Schedule() below a second time.
           The task comes back from Schedule() with interrupts still off,
           which the switch saves and restores per task. */
        ulong flags = Hal::IrqSave();
        self->SleepUntil.Set((long)until);
        self->Block();
        Schedule();
        self->Unblock();
        self->SleepUntil.Set(0);
        Hal::IrqRestore(flags);
    }
}

}