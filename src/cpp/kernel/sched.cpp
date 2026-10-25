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

/* Runnable tasks reported for having waited TaskQueue::StallReportNs for
   their CPU (TaskQueue::CheckStalls) */
static Atomic StallReports;

long GetStallReportCount()
{
    return StallReports.Get();
}

void SchedStats::Bump(ulong& counter, ulong by)
{
    __atomic_store_n(&counter, __atomic_load_n(&counter, __ATOMIC_RELAXED) + by, __ATOMIC_RELAXED);
}

void SchedStats::Raise(ulong& max, ulong value)
{
    if (__atomic_load_n(&max, __ATOMIC_RELAXED) < value)
        __atomic_store_n(&max, value, __ATOMIC_RELAXED);
}

ulong SchedStats::Load(const ulong& counter)
{
    return __atomic_load_n(&counter, __ATOMIC_RELAXED);
}

ulong SchedStats::Bucket(ulong ns)
{
    ulong us = ns / Const::NanoSecsInUsec;
    if (us == 0)
        return 0;

    ulong bucket = Stdlib::SizeOfInBits<ulong>() - (ulong)__builtin_clzl(us);
    return Stdlib::Min(bucket, Buckets - 1);
}

ulong SchedStats::BucketLimitUs(ulong bucket)
{
    return 1UL << Stdlib::Min(bucket, Buckets - 1);
}

void SchedStats::Wake(ulong ns)
{
    Bump(WakeHist[Bucket(ns)], 1);
    Raise(WakeMax, ns);
}

void SchedStats::Delay(ulong ns)
{
    Bump(DelayHist[Bucket(ns)], 1);
}

void SchedStats::Decision(ulong cycles, ulong touched)
{
    Bump(Decisions, 1);
    Bump(DecisionCycles, cycles);
    Raise(DecisionMax, cycles);
    Bump(Touched, touched);
    Raise(TouchedMax, touched);
}

void SchedStats::Look(ulong runnable)
{
    Bump(Looks, 1);
    Bump(RunnableSum, runnable);
    Raise(RunnableMax, runnable);
    if (runnable >= 2)
        Bump(Crowded, 1);
}

void SchedStats::Read(Snapshot& out)
{
    for (ulong i = 0; i < Buckets; i++)
    {
        out.Wake[i] = Load(WakeHist[i]);
        out.Delay[i] = Load(DelayHist[i]);
    }
    out.WakeMaxNs = Load(WakeMax);
    out.Decisions = Load(Decisions);
    out.DecisionCycles = Load(DecisionCycles);
    out.DecisionMaxCycles = Load(DecisionMax);
    out.Touched = Load(Touched);
    out.TouchedMax = Load(TouchedMax);
    out.Looks = Load(Looks);
    out.Runnable = Load(RunnableSum);
    out.RunnableMax = Load(RunnableMax);
    out.Crowded = Load(Crowded);
}

void TaskQueue::ReadStats(SchedStats::Snapshot& out)
{
    Stats.Read(out);
}

TaskQueue::TaskQueue()
    : StallPending(false)
    , StallName{}
    , StallPid(0)
    , StallWaitedMs(0)
    , StallRunnable(0)
    , IdleCounted(false)
    , IdleTask(nullptr)
{
    Stdlib::AutoLock lock(Lock);
    TaskList.Init();
    SleepList.Init();
    ExitedList.Init();

    WakeHead.Set(0);
    SwitchContextCounter.Set(0);
    ScheduleCounter.Set(0);
    TaskCount.Set(0);
    RunnableCount.Set(0);
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

    ReportStall();

    if (prev->State.Get() != Task::StateExited)
    {
        /* Only one that can still run -- displaced, not asleep -- is
           weighed for a lighter CPU. One asleep stays on the queue its
           wakeups go to (Task::Unblock) and whose timer is armed for its
           deadline; it moves, if it should, once it runs. Its state cannot
           change under this: a Queued task is moved from there only by
           itself, and it is not running. */
        bool runnable = prev->RunState.Get() == Task::RunQueued && !prev->IsIdle();
        auto taskQueue = runnable ? prev->SelectNextTaskQueue() : nullptr;

        /* Schedule()'s count should be the only one left: the locks
           TaskQueue::Schedule took in prev were released above, in this
           task, and handed their counts back to prev -- a RawSpinLock
           remembers whose preemption it disabled. Anything more would be a
           lock prev still holds, which Schedule() never switches away from,
           or a count gone astray. Checked while that last count still keeps
           prev off every CPU. */
        BugOn(prev->PreemptDisableCounter.Get() != 1);

        if (taskQueue != nullptr)
        {
            /* Off this queue, its count down while it is on none, and only
               then onto the other: a decision never finds a task whose count
               is up -- here, prev's comes down before this CPU decides
               again -- so none has one to pass over. It used to be inserted
               there first, and every walk had to skip a task whose count was
               up. */
            TaskMigrations.Inc();
            prev->Get();
            prev->TaskQueue->Remove(prev);
            prev->PreemptDisableCounter.Dec();
            taskQueue->Insert(prev);
            prev->Put();
        }
        else
        {
            prev->PreemptDisableCounter.Dec();
        }
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

void TaskQueue::NoteRun(Task* next, ulong now)
{
    /* The idle task waits for nothing: it runs when nobody else can */
    if (next->IsIdle())
        return;

    /* When it left the CPU, or started: UpdateRuntime stamped it then, and
       it has not run since */
    ulong out = next->RunStartTime.GetValue();

    if (next->SwitchedOutRunnable)
    {
        if (now > out)
            Stats.Delay(now - out);
        return;
    }

    /* Whoever woke it said when: an Unblock(), or its deadline (WakeDue) --
       unless that was before it left the CPU, a wakeup for the wait before
       this one */
    ulong woke = (ulong)next->WakeTime.Get();
    if (woke != 0 && woke >= out && now >= woke)
        Stats.Wake(now - woke);
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
    Stdlib::Time now = GetBootTime();
    NoteRun(next, now.GetValue());
    next->RunStartTime = now;
    next->Sched.ExecStart = now.GetValue();
    next->Prev = curr;
    SwitchContext(next->Rsp, &curr->Rsp, &TaskQueue::SwitchComplete, next);
}

/* A Treiber stack that is only ever pushed onto and taken whole, never
   popped: there is no ABA -- a task pushed again after the take is a new
   entry on what was an empty stack -- and the push is all a waker does, from
   whatever context it is in. The compare-and-swap that publishes the entry
   is sequentially consistent, so the WakeNext written before it is seen by
   the CPU that takes the stack. A task is on one stack at a time: only the
   Unblock() that made it Waking pushes it, and only DrainWakes takes it off
   that state. */
void TaskQueue::PushWake(Task* task)
{
    long head = WakeHead.Get();
    for (;;)
    {
        task->WakeNext = reinterpret_cast<Task*>(head);
        long seen = WakeHead.Cmpxchg(reinterpret_cast<long>(task), head);
        if (seen == head)
            return;
        head = seen;
    }
}

Task* TaskQueue::TaskOf(SchedEntity* se)
{
    return (se != nullptr) ? CONTAINING_RECORD(se, Task, Sched) : nullptr;
}

void TaskQueue::Account(Task* curr, ulong now, bool exited)
{
    /* On the CPU is curr, or nothing counted: the idle task idling, or an
       exited task gone already, at a look before this one */
    SchedEntity* se = &curr->Sched;
    BugOn(Eevdf.Current() != nullptr && Eevdf.Current() != se);

    if (Eevdf.Current() == se)
    {
        if (exited)
            Eevdf.Leave(se);
        else
            Eevdf.Charge(now);
    }
    CountIdle(curr, now);
}

void TaskQueue::CountIdle(Task* curr, ulong now)
{
    if (curr != IdleTask)
        return;

    if (curr->IsIdling() && IdleCounted)
    {
        Eevdf.Leave(&curr->Sched);
        IdleCounted = false;
    }
    else if (!curr->IsIdling() && !IdleCounted)
    {
        Eevdf.JoinCurrent(&curr->Sched);
        curr->Sched.ExecStart = now;
        IdleCounted = true;
    }
}

void TaskQueue::ArmSlice(ulong now)
{
    /* Alone on the CPU, the current one's slice ends deciding nothing: it
       would keep the CPU. A task that joins later is a decision of its own
       -- a wakeup's, a deadline's, the tick's -- which arms it then. A
       guest's vCPU on a core of its own would otherwise leave its guest
       every slice for nothing. */
    if (Eevdf.Current() == nullptr || Eevdf.IsEmpty())
        return;

    ulong ns = Stdlib::Max(Eevdf.RealUntilDeadline(), MinArmNs);
    CpuTable::GetInstance().GetCurrentCpu().LocalTimerWake(now + ns);
}

void TaskQueue::Enqueue(Task* task)
{
    RunnableCount.Inc();
    Eevdf.Join(&task->Sched);
    NoteQueued(task);
}

void TaskQueue::Requeue(Task* task)
{
    BugOn(Eevdf.Current() != &task->Sched);
    Eevdf.PutBack();
    NoteQueued(task);
}

void TaskQueue::NoteQueued(Task* task)
{
    task->QueuedAt = GetBootTime().GetValue();
    task->StallReported = false;
}

void TaskQueue::Asleep(Task* task)
{
    /* The lag it goes with, V counting it still: it is current */
    BugOn(Eevdf.Current() != &task->Sched);
    Eevdf.Leave(&task->Sched);

    RunnableCount.Dec();
    BugOn(RunnableCount.Get() < 0);

    ulong until = (ulong)task->SleepUntil.Get();
    if (until == 0)
        return;

    /* Soonest first. From the tail, where a later deadline mostly goes; the
       list is as long as this CPU has sleepers, a handful */
    auto* pos = SleepList.Blink;
    while (pos != &SleepList &&
           (ulong)CONTAINING_RECORD(pos, Task, SleepEntry)->SleepUntil.Get() > until)
        pos = pos->Blink;

    /* After pos, which is before the one that follows it */
    pos->Flink->InsertTail(&task->SleepEntry);
}

void TaskQueue::DrainWakes(ulong& touched)
{
    if (WakeHead.Get() == 0)
        return;

    /* The whole stack at once. Only this CPU takes it to 0, so it is not 0
       again until it is taken. */
    long head = WakeHead.Get();
    for (;;)
    {
        long seen = WakeHead.Cmpxchg(0, head);
        if (seen == head)
            break;
        head = seen;
    }

    /* The one pushed last is on top: turned over, they take their turns in
       the order they were woken */
    Task* order = nullptr;
    for (Task* task = reinterpret_cast<Task*>(head); task != nullptr;)
    {
        Task* below = task->WakeNext;
        task->WakeNext = order;
        order = task;
        task = below;
    }

    while (order != nullptr)
    {
        Task* task = order;
        order = task->WakeNext;
        task->WakeNext = nullptr;
        touched++;

        /* Off its CPU for good before it can be pushed: it was made
           Sleeping by the Schedule() that switched it out on this CPU, whose
           next one this is */
        BugOn(task->TaskQueue != this);
        BugOn(task->State.Get() == Task::StateRunning);

        /* Woken before its deadline: off the sleep list too */
        if (!task->SleepEntry.IsEmpty())
            task->SleepEntry.RemoveInit();

        long was = task->RunState.Cmpxchg(Task::RunQueued, Task::RunWaking);
        BugOn(was != Task::RunWaking);
        Enqueue(task);
    }
}

void TaskQueue::WakeDue(ulong now, ulong& touched)
{
    while (!SleepList.IsEmpty())
    {
        Task* task = CONTAINING_RECORD(SleepList.Flink, Task, SleepEntry);
        ulong until = (ulong)task->SleepUntil.Get();
        if (until > now)
            return;

        task->SleepEntry.RemoveInit();
        touched++;

        /* Unless an Unblock() won it first: then it is Waking, on the wake
           stack, and the next drain puts it on the run list */
        if (task->RunState.Cmpxchg(Task::RunQueued, Task::RunSleeping) == Task::RunSleeping)
        {
            task->WakeTime.Set((long)until);
            Enqueue(task);
        }
    }
}

void TaskQueue::ReleaseIdle(Task* curr)
{
    if (IdleTask == nullptr || IdleTask->IsIdle())
        return;

    /* Counted from now on, as any task that can run. Counted already -- on
       the CPU or on the run list, running something other than its idling
       -- it stays where it is; idling on the CPU, it is placed as a task
       waking, and takes the run list when it leaves the CPU; in its place
       as the last resort, it is placed and on the run list now. */
    Task* task = IdleTask;
    IdleTask = nullptr;
    bool counted = IdleCounted;
    IdleCounted = false;

    if (counted)
    {
        RunnableCount.Inc();
    }
    else if (task == curr)
    {
        RunnableCount.Inc();
        Eevdf.JoinCurrent(&task->Sched);
        task->Sched.ExecStart = GetBootTime().GetValue();
    }
    else
    {
        Enqueue(task);
    }
}

void TaskQueue::CheckStalls(ulong now)
{
    /* The run list is in no order of waiting: the one waiting longest is
       looked for, the list as long as this CPU has tasks that can run */
    Task* task = nullptr;
    auto& list = Eevdf.Entries();
    for (auto entry = list.Flink; entry != &list; entry = entry->Flink)
    {
        Task* cand = TaskOf(CONTAINING_RECORD(entry, SchedEntity, Entry));
        if (!cand->StallReported && (task == nullptr || cand->QueuedAt < task->QueuedAt))
            task = cand;
    }
    if (task == nullptr || now < task->QueuedAt + StallReportNs)
        return;

    /* Said once the locks are down (ReportStall): the trace path ends in the
       console and, with disklog=on, in a disk write, and none of that is for
       under the queue's lock */
    task->StallReported = true;
    StallReports.Inc();
    Stdlib::SnPrintf(StallName, sizeof(StallName), "%s", task->GetName());
    StallPid = task->Pid;
    StallWaitedMs = (now - task->QueuedAt) / Const::NanoSecsInMs;
    StallRunnable = RunnableCount.Get();
    StallPending = true;
}

void TaskQueue::ReportStall()
{
    if (likely(!StallPending))
        return;

    StallPending = false;
    Trace(0, "sched: %s (pid %lu) could run, and has waited %lu ms for cpu %lu, with %ld that can run there",
        StallName, StallPid, StallWaitedMs, CpuTable::GetInstance().GetCurrentCpuId(), StallRunnable);
}

void TaskQueue::Schedule(Task* curr, ScheduleReason why)
{
    ScheduleCounter.Inc();

    ulong flags = Hal::IrqSave();
    u64 began = Hal::ReadCycleCounter();

    /* Whatever reschedule was put off until now, this is it */
    curr->PreemptPending.Set(0);
    Lock.Lock();

    /* The timer fired since the last look: the queue is counted for
       SchedStats and looked at for a task left waiting too long */
    auto& cpu = CpuTable::GetInstance().GetCurrentCpu();
    bool fired = cpu.LocalTimerTakeWalk();

    /* Only the idle task to hand the CPU to: a task polling with
       YieldToRunnable, or one an interrupt landed on, keeps it instead */
    bool keepOverIdle = (why != ScheduleReason::Voluntary);

    Task* next = nullptr;
    bool nextQueued = false;
    bool exited = false;
    long run = Task::RunQueued;
    ulong now = 0;
    ulong exitedRetries = 0;
    ulong touched = 1;
    for (;;) {
        curr->Lock.Lock();
        BugOn(TaskList.IsEmpty());

        /* curr charged for what it ran, gone from the count if it has
           exited, and the idle task in it or out of it as it runs something
           or idles: V is the queue's as it is now before anything joins it */
        now = GetBootTime().GetValue();
        exited = (curr->State.Get() == Task::StateExited);
        ReleaseIdle(curr);
        Account(curr, now, exited);

        /* What can run that could not at the last look: the woken, and the
           sleepers whose time has come, each placed by the lag it brings */
        DrainWakes(touched);
        if (!SleepList.IsEmpty())
            WakeDue(now, touched);

        if (exited && curr->TaskQueue != nullptr)
        {
            BugOn(curr->TaskQueue != this);
            BugOn(curr->ListEntry.IsEmpty());
            curr->TaskQueue = nullptr;
            curr->ListEntry.RemoveInit();
            TaskCount.Dec();
            BugOn(TaskCount.Get() < 0);
            RunnableCount.Dec();
            BugOn(RunnableCount.Get() < 0);
        }

        /* On its CPU, a task is Queued, or Blocking on its way to sleep:
           only this CPU makes it anything else, and only as it leaves */
        run = curr->RunState.Get();
        BugOn(!exited && run != Task::RunQueued && run != Task::RunBlocking);
        bool runnable = !exited && run == Task::RunQueued;

        SchedEntity* current = Eevdf.Current();
        if (!Eevdf.IsEmpty())
        {
            SchedEntity* best;
            if (runnable && why == ScheduleReason::Preempt && current != nullptr)
            {
                /* An interrupt's: curr among the candidates, and it keeps
                   the CPU unless another eligible task's deadline comes
                   sooner -- its slice done, or a wakeup owed more */
                best = Eevdf.Pick(true);
                next = (best == current) ? nullptr : TaskOf(best);
            }
            else
            {
                /* curr lets the others go -- yields, sleeps, exits -- or is
                   the idle task idling, which keeps nothing from anybody:
                   the eligible one whose deadline is soonest of the rest. A
                   task that can still run goes behind them first. */
                if (runnable && current != nullptr)
                    Eevdf.Yield();
                best = Eevdf.Pick(false);
                next = TaskOf(best);
            }
            /* On the run list until curr has left the CPU (below): the
               queue takes it only then */
            nextQueued = (next != nullptr);
        }
        else if (!(runnable && keepOverIdle) && IdleTask != nullptr && IdleTask != curr)
        {
            /* The idle task takes its turn only when there is no other turn
               to take, and not over a task an interrupt landed on, or one
               polling with YieldToRunnable, that can still run itself. It
               used to sit in the walk as an equal, so a reschedule -- and
               every raise of a softirq from an interrupt handler forces one,
               by way of the IPI -- could pick it over the task that had just
               been given work. It then halted the CPU, and the work waited
               for whatever interrupt came next. */
            next = IdleTask;
        }

        /* Nothing to switch to: curr keeps the CPU -- runnable, or blocking
           with no idle task to go to, when it comes back from Schedule()
           still Blocking, unblocks itself, and looks again */
        if (next != nullptr || !exited)
            break;

        /* An exited task can't be returned to: drop the locks and wait
           for a runnable candidate (the idle task at the latest) */
        curr->Lock.Unlock();
        Lock.Unlock();
        if (++exitedRetries > MaxExitedRetries)
            Panic("Schedule: exited task but no runnable candidate");
        Pause();
        Lock.Lock();
    }

    if (next != nullptr)
    {
        next->Lock.Lock();

        /* Nothing that could not run is ever on the run list, and no task
           with preemption disabled is ever on one when a decision is made
           (SwitchComplete): what was passed over is now a report */
        BugOn(next->RunState.Get() != Task::RunQueued);
        BugOn(next->PreemptDisableCounter.Get() != 0);

        /* curr leaves the CPU. Blocking, it goes to sleep -- unless an
           Unblock() made it Queued since it looked, and then it has not.
           Runnable, it goes back on the run list -- the idle task too, while
           it runs the boot or a test and is counted, and only while it idles
           is its place its own (Task::SetIdling). Kept apart whatever it ran,
           it used to be the last resort with the boot in it: a task the boot
           started on its own CPU that never blocked -- the ring selftest's
           producer -- took that CPU at the first tick and kept it, and the
           boot never got as far as starting the consumer the producer waited
           for. */
        if (!exited)
        {
            if (run == Task::RunBlocking &&
                curr->RunState.Cmpxchg(Task::RunSleeping, Task::RunBlocking) == Task::RunBlocking)
            {
                Asleep(curr);
                curr->SwitchedOutRunnable = false;
            }
            else
            {
                if (Eevdf.Current() != nullptr)
                    Requeue(curr);
                curr->SwitchedOutRunnable = true;
            }
        }

        /* The idle task, as the last resort, is not counted and takes
           nothing */
        if (nextQueued)
            Eevdf.Take(&next->Sched);
    }

    /* This CPU's timer at the soonest sleeper's deadline -- curr's, if it has
       just gone to sleep until one -- so that the sleeper runs then, and not
       at the next tick. A deadline later than the timer has is no change. */
    if (!SleepList.IsEmpty())
        cpu.LocalTimerWake((ulong)CONTAINING_RECORD(SleepList.Flink, Task, SleepEntry)->SleepUntil.Get());

    /* And at the end of the slice of whoever runs now, so that the next
       decision comes when its deadline does */
    ArmSlice(now);

    if (fired)
    {
        Stats.Look((ulong)RunnableCount.Get());
        CheckStalls(now);
    }

    /* What deciding cost, the locks' waits included, and not the switch */
    Stats.Decision(Hal::ReadCycleCounter() - began, touched);

    if (next == nullptr)
    {
        curr->UpdateRuntime();
        curr->Lock.Unlock();
        Lock.Unlock();

        ReportStall();

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
    BugOn(!task->Sched.Entry.IsEmpty());
    BugOn(task->RunState.Get() != Task::RunQueued);

    task->TaskQueue = this;
    TaskList.InsertTail(&task->ListEntry);
    TaskCount.Inc();

    /* The idle task has a place of its own, apart from the run list. It is
       made one before it is inserted (Cpu::Run), and is already running:
       Task::Run inserts it from its own stack. */
    if (task->IsIdle())
    {
        BugOn(IdleTask != nullptr);
        IdleTask = task;
        return;
    }

    Enqueue(task);
}

void TaskQueue::Remove(Task* task)
{
    {
        Stdlib::AutoLock lock(Lock);
        Stdlib::AutoLock lock2(task->Lock);

        BugOn(task->TaskQueue != this);
        BugOn(task->ListEntry.IsEmpty());

        /* Only a task that can run is moved (SwitchComplete): one asleep
           stays where its wakeups and its deadline go, and the idle task
           where it is */
        BugOn(task->RunState.Get() != Task::RunQueued);
        BugOn(task == IdleTask);
        BugOn(!task->SleepEntry.IsEmpty());

        /* It takes its lag with it, to be placed by on the queue it goes to.
           Not current: displaced, and back on the run list, where the
           decision that switched it out put it (SwitchComplete moves it). */
        BugOn(Eevdf.Current() == &task->Sched);
        Eevdf.Leave(&task->Sched);
        RunnableCount.Dec();
        BugOn(RunnableCount.Get() < 0);

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
        RunnableCount.Set(0);
        IdleTask = nullptr;
        WakeHead.Set(0);

        /* What waits on the run and sleep lists is on the task list too, and
           goes with it: unlinked, so that no task is left pointing into
           lists that are gone */
        Eevdf.Clear();
        IdleCounted = false;
        while (!SleepList.IsEmpty())
            SleepList.RemoveHead()->Init();
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

    Trace(0, "TaskQueue 0x%p counters: sched %lu switch context %lu",
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

long TaskQueue::GetRunnableCount()
{
    return RunnableCount.Get();
}

long TaskQueue::GetSwitchContextCounter()
{
    return SwitchContextCounter.Get();
}

static void ScheduleCurrent(ScheduleReason why)
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

    curr->TaskQueue->Schedule(curr, why);
}

void Schedule()
{
    ScheduleCurrent(ScheduleReason::Voluntary);
}

void Preempt()
{
    ScheduleCurrent(ScheduleReason::Preempt);
}

void YieldToRunnable()
{
    ScheduleCurrent(ScheduleReason::Yield);
}

bool TaskQueue::SetNice(Task* task, long nice)
{
    /* The task's lock too, in the scheduler's order: TaskQueue is written
       under it everywhere, Clear's included, which does not hold this one */
    Stdlib::AutoLock lock(Lock);
    Stdlib::AutoLock lock2(task->Lock);
    if (task->TaskQueue != this)
        return false;

    /* Its weight from now on: V and every eligibility are reckoned from
       scratch at the next decision, and it is charged at it from then */
    task->Sched.SetNice(nice);
    return true;
}

ulong TaskQueue::View(EntityView* out, ulong max)
{
    Stdlib::AutoLock lock(Lock);

    SchedEntity* curr = Eevdf.Current();
    u64 v = Eevdf.V();
    ulong count = 0;

    /* curr first, then the list */
    SchedEntity* first = curr;
    auto& list = Eevdf.Entries();
    Stdlib::ListEntry* entry = list.Flink;
    for (;;)
    {
        SchedEntity* se;
        if (first != nullptr)
        {
            se = first;
            first = nullptr;
        }
        else if (entry != &list)
        {
            se = CONTAINING_RECORD(entry, SchedEntity, Entry);
            entry = entry->Flink;
        }
        else
        {
            break;
        }

        if (count == max)
            break;

        Task* task = TaskOf(se);
        EntityView& e = out[count++];
        Stdlib::SnPrintf(e.Name, sizeof(e.Name), "%s", task->GetName());
        e.Pid = task->Pid;
        e.Nice = se->Nice;
        e.SliceNs = se->SliceNs;
        e.Lag = (s64)(v - se->VRuntime);
        e.Deadline = (s64)(se->Deadline - v);
        e.Eligible = Eevdf.IsEligible(se);
        e.OnCpu = (se == curr);
    }
    return count;
}

bool TaskQueue::SetSlice(Task* task, ulong sliceNs)
{
    Stdlib::AutoLock lock(Lock);
    Stdlib::AutoLock lock2(task->Lock);
    if (task->TaskQueue != this)
        return false;

    /* The next deadline it is given is a slice of this past its VRuntime */
    task->Sched.SetSlice(sliceNs);
    return true;
}

/* The queue a task is on is read under the task's lock, and looked at again
   under both, the queue's first: the order the scheduler takes them in, so
   the task's is let go before the queue's is taken. A task on no queue --
   not started, or exited -- is set under its own lock, which Insert takes
   too. */
static TaskQueue* QueueOf(Task* task)
{
    Stdlib::AutoLock lock(task->Lock);
    return task->TaskQueue;
}

void SetTaskNice(Task* task, long nice)
{
    for (;;)
    {
        TaskQueue* queue = QueueOf(task);
        if (queue == nullptr)
        {
            Stdlib::AutoLock lock(task->Lock);
            if (task->TaskQueue == nullptr)
            {
                task->Sched.SetNice(nice);
                return;
            }
            continue;
        }
        if (queue->SetNice(task, nice))
            return;
    }
}

void SetTaskSlice(Task* task, ulong sliceNs)
{
    for (;;)
    {
        TaskQueue* queue = QueueOf(task);
        if (queue == nullptr)
        {
            Stdlib::AutoLock lock(task->Lock);
            if (task->TaskQueue == nullptr)
            {
                task->Sched.SetSlice(sliceNs);
                return;
            }
            continue;
        }
        if (queue->SetSlice(task, sliceNs))
            return;
    }
}

/* Sleep blocks. The task is off its queue's run list until its time has
   come, on the queue's sleep list, soonest first, and the scheduling point
   its CPU's timer makes at the deadline -- armed for it as the task goes to
   sleep -- or any sooner one that finds the time has come puts it back
   (TaskQueue::WakeDue). That used to be the tick's, 100 times a second, at the
   latest, and a sleep of a millisecond took ten. It used to poll, too, a loop
   around Schedule() that left it runnable: alone on its CPU it let the CPU
   halt until the tick, but two on one CPU handed it to each other without
   end and the idle task never ran. Two idle hypervisor guests, each vCPU
   asleep until its guest's next timer edge, kept a CPU of the AX41 busy that
   way between them, 46% each.

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