#pragma once

#include "spin_lock.h"
#include "task.h"

#include <lib/stdlib.h>
#include <lib/list_entry.h>

namespace Kernel
{

/* What one CPU's scheduler did, counted as it does it: how long a task that
   was woken waited for the CPU, how long one displaced while it could still
   run waited to run again, what a decision cost, how far its walk went, and
   how many tasks could run each time the CPU's timer had the queue looked
   at. Written only by the CPU that owns the queue, under its lock; read
   from anywhere with no lock, a counter at a time (`sched`), so a reading
   is a moment's and not a consistent whole. */
class SchedStats final
{
public:
    /* Log2 buckets of microseconds: bucket 0 is under 1 us, bucket k is
       [2^(k-1), 2^k) us, and the last holds everything from 2^(Buckets-2) us
       up -- about 4 s */
    static const ulong Buckets = 24;

    struct Snapshot
    {
        /* Woken: unblocked by someone, or asleep until a deadline that
           came -- from then until it ran */
        ulong Wake[Buckets];
        ulong WakeMaxNs;
        /* Displaced while it could still run, until it ran again */
        ulong Delay[Buckets];
        ulong Decisions;
        ulong DecisionCycles;
        ulong DecisionMaxCycles;
        /* Entries the walk looked at, summed over the decisions */
        ulong Walked;
        ulong WalkMax;
        /* Looks at the queue the timer made, the tasks they found able to
           run -- the one on the CPU included, the idle task not -- summed,
           the most, and how many found two or more */
        ulong Looks;
        ulong Runnable;
        ulong RunnableMax;
        ulong Crowded;
    };

    constexpr SchedStats() = default;
    ~SchedStats() = default;

    void Wake(ulong ns);
    void Delay(ulong ns);
    void Decision(ulong cycles, ulong walked);
    void Look(ulong runnable);

    void Read(Snapshot& out);

    static ulong Bucket(ulong ns);
    /* What bucket's entries are under, in microseconds */
    static ulong BucketLimitUs(ulong bucket);

private:
    SchedStats(const SchedStats& other) = delete;
    SchedStats(SchedStats&& other) = delete;
    SchedStats& operator=(const SchedStats& other) = delete;
    SchedStats& operator=(SchedStats&& other) = delete;

    /* One writer, so a relaxed load and a relaxed store and no locked
       instruction -- Atomic's Set is a lock xchg on x86, five of them a
       decision: a reader on another CPU sees the value before or the value
       after, never a torn one */
    static void Bump(ulong& counter, ulong by);
    static void Raise(ulong& max, ulong value);
    static ulong Load(const ulong& counter);

    ulong WakeHist[Buckets] = {};
    ulong WakeMax = 0;
    ulong DelayHist[Buckets] = {};
    ulong Decisions = 0;
    ulong DecisionCycles = 0;
    ulong DecisionMax = 0;
    ulong Walked = 0;
    ulong WalkMax = 0;
    ulong Looks = 0;
    ulong RunnableSum = 0;
    ulong RunnableMax = 0;
    ulong Crowded = 0;
};

/* Why a decision is made: a voluntary Schedule() -- curr blocking, exiting,
   or letting others run; YieldToRunnable() -- curr polling, and giving the
   CPU to whoever else can use it, never to the idle task; Preempt() -- an
   interrupt's, the tick, a slice's end, an IPI for a wakeup, where curr
   keeps the CPU unless another is due it */
enum class ScheduleReason
{
    Voluntary,
    Yield,
    Preempt,
};

class TaskQueue
{
public:
    TaskQueue();
    ~TaskQueue();

    void Insert(Task* task);
    void Remove(Task* task);

    void Schedule(Task* curr, ScheduleReason why);

    /* A task's nice and slice (SchedEntity), set under this queue's lock --
       false when the task is not this queue's, and the caller looks again
       (SetTaskNice, SetTaskSlice) */
    bool SetNice(Task* task, long nice);
    bool SetSlice(Task* task, ulong sliceNs);

    /* What EEVDF has of one task on this queue: `sched queue` */
    struct EntityView
    {
        char Name[TaskNameLen];
        ulong Pid;
        long Nice;
        ulong SliceNs;
        /* V less its VRuntime, and its deadline less V: virtual ns */
        s64 Lag;
        s64 Deadline;
        bool Eligible;
        bool OnCpu;
    };

    /* The queue as it is, copied under its lock into out -- the one on the
       CPU first, if it is counted -- for printing after: at most max, the
       count returned */
    ulong View(EntityView* out, ulong max);

    void Clear();

    long GetSwitchContextCounter();

    /* How many tasks this queue is home to -- runnable, running, asleep, the
       idle task -- maintained by every change of home */
    long GetTaskCount();

    /* How many of them could run now: the one on the CPU included, the idle
       task not. What the load balancer weighs (Task::SelectNextTaskQueue). */
    long GetRunnableCount();

    /* Free tasks that exited on this queue's CPU. Must be called with
       interrupts enabled (it frees stacks, which triggers a blocking TLB
       shootdown) -- see SwitchComplete. */
    void ReapExited();

    void ReadStats(SchedStats::Snapshot& out);

    /* A task this queue is home to, found asleep by an Unblock() that has
       just made it Task::RunWaking: handed back from any CPU and any
       context, a hard IRQ handler's included, without a lock -- pushed onto
       a stack this queue's CPU takes whole at its next Schedule() and puts
       on the run list (DrainWakes). That Schedule() is the waker's to bring
       about, as it always was: Event::Signal and SoftIrq::Raise send the IPI. */
    void PushWake(Task* task);

private:
    TaskQueue(const TaskQueue &other) = delete;
    TaskQueue(TaskQueue&& other) = delete;
    TaskQueue& operator=(const TaskQueue& other) = delete;
    TaskQueue& operator=(TaskQueue&& other) = delete;

    /* Everything below is under Lock, interrupts off, on this queue's CPU */

    /* The tasks woken since the last look (PushWake), onto the run list in
       the order they were woken; touched counts them, for SchedStats */
    void DrainWakes(ulong& touched);

    /* The sleepers whose deadline is not after now, onto the run list */
    void WakeDue(ulong now, ulong& touched);

    /* A task that could not run and now can -- new, woken, moved here:
       placed by its lag (EevdfQueue::Join), onto the run list, and counted */
    void Enqueue(Task* task);

    /* curr, current and displaced while it can still run: back on the run
       list where its virtual runtime and deadline say */
    void Requeue(Task* task);

    /* A task has gone on the run list now: what CheckStalls times */
    void NoteQueued(Task* task);

    /* curr, gone to sleep: its lag kept, no longer counted, and on the
       sleep list if it is asleep until a deadline */
    void Asleep(Task* task);

    /* curr as a decision finds it: charged for what it has run at its
       weight if it is current, gone from the queue if it has exited, and
       the idle task in the count or out of it (CountIdle) */
    void Account(Task* curr, ulong now, bool exited);

    /* The idle task joins the count when it stops idling -- placed as a
       task waking, from the lag it left with -- and leaves it, its lag kept,
       when it idles again: idling, it is not one of the tasks that share the
       CPU, only what runs when none of them can */
    void CountIdle(Task* curr, ulong now);

    static Task* TaskOf(SchedEntity* se);

    /* The slice the current one is given, when another waits for the CPU:
       this CPU's timer brought forward to when its deadline comes, so that
       its slice ends then and not at the next tick. No sooner than MinArmNs
       from now. */
    void ArmSlice(ulong now);

    static const ulong MinArmNs = 50 * Const::NanoSecsInUsec;

    /* The idle task made an ordinary one (Task::ClearIdle, at a shutdown):
       out of its place as the last resort, and into its turn */
    void ReleaseIdle(Task* curr);

    /* A runnable task that has waited StallReportNs for this CPU is a
       report, once a wait: a scheduler that can starve one says so. Noted
       under the lock, and said by ReportStall once this CPU holds none of
       the scheduler's locks -- the keep path's end, or SwitchComplete. */
    void CheckStalls(ulong now);
    void ReportStall();

    static const ulong StallReportNs = 5 * Const::NanoSecsInSec;

    /* The stall noted and not yet said: this CPU's alone */
    bool StallPending;
    char StallName[TaskNameLen];
    ulong StallPid;
    ulong StallWaitedMs;
    long StallRunnable;

    /* Spins allowed in Schedule while an exited task waits for a
       runnable candidate before declaring the queue broken */
    static const ulong MaxExitedRetries = 100000000;

    void Switch(Task* next, Task* curr);

    void SwitchComplete(Task* curr);

    static void SwitchComplete(void* ctx);

    using ListEntry = Stdlib::ListEntry;

    /* Every task this queue is home to (Task::ListEntry) */
    ListEntry TaskList;

    /* The runnable ones (Task::Sched.Entry), the one on the CPU among them
       as the queue's current, and the rules a decision over them is made
       by: EEVDF (kernel/eevdf.h). The idle task is one of them only while it
       runs something other than its idling. Only runnable tasks: one blocked
       is off it until woken, so a decision never passes over a task that
       cannot run. */
    EevdfQueue Eevdf;

    /* The idle task is counted -- it runs something other than its idling,
       or waits for its turn to (CountIdle) */
    bool IdleCounted;

    /* The tasks asleep until a deadline -- Sleep(), Event::WaitFor() --
       soonest first (Task::SleepEntry): what this CPU's timer is armed for,
       and what WakeDue looks at the head of */
    ListEntry SleepList;

    /* The CPU's idle task, kept apart from the run list: the last resort,
       taken only when nothing else can run */
    Task* IdleTask;

    /* PushWake's stack, a Task* and 0 for none, linked by Task::WakeNext */
    Atomic WakeHead;

    SpinLock Lock;

    /* Tasks that exited on this CPU, pending a stack free with interrupts
       enabled (ReapExited), rather than in the IRQs-off switch path. */
    ListEntry ExitedList;
    SpinLock ExitedLock;

    Atomic ScheduleCounter;
    Atomic SwitchContextCounter;
    Atomic TaskCount;
    Atomic RunnableCount;

    /* The waits next has just ended, as it is switched in at now: woken,
       or displaced while runnable (SchedStats). Under Lock. */
    void NoteRun(Task* next, ulong now);

    SchedStats Stats;
};


/* The reschedule for a task that has already taken itself out of the walk:
   blocked (Event::Wait), exited (Task::Exit), or sleeping long enough that
   halting the CPU cannot overshoot the deadline (Sleep). When nothing else
   is runnable the CPU goes to the idle task, which halts it until the next
   interrupt that CPU itself takes -- often nothing nearer than its tick.

   A task that is still runnable must not poll here. Nothing wakes it: the
   thing it waits for is a store or a counter in somebody else's interrupt
   handler, with no waiter to unblock and no IPI to send, so it waits out the
   tick with its work already done. Poll with YieldToRunnable(), which gives
   the CPU to any other runnable task but never to the idle one. Every wait
   primitive here does -- WaitGroup, Mutex, RwMutex -- and the one time each
   of them did not, it cost a tick per wait (see WaitGroup::Wait). */
void Schedule();

/* The reschedule an interrupt asks for -- the tick's and every IPI's: the CPU
   goes to another task runnable on it, if there is one, and otherwise stays
   with the task the interrupt landed on. Never to the idle task while that
   task can still run, as Schedule() would: the idle task only halts, and the
   task it displaced, runnable rather than blocked, gets no IPI from whoever
   has work for it -- it waited for the next tick. On the AX41 that took
   netblk's polling worker off its CPU some 25 times a second, a tick each:
   a quarter of the CPU, and a p99 of 10 ms. A task with preemption off keeps
   the CPU, and the reschedule is made when its PreemptEnable() brings the
   count back to zero -- deferred, not dropped (see ScheduleCurrent). */
void Preempt();

/* Gives the CPU to another task runnable on it, if there is one, and returns
   at once if there is not: never to the idle task, as Schedule() would, which
   halts the CPU until the next interrupt that CPU takes. The way to poll for
   work another CPU's interrupt will bring, without keeping whatever else is
   runnable here -- a softirq task the tick preempted mid-handler among them
   -- off the CPU the way a plain spin does. */
void YieldToRunnable();

void Sleep(ulong nanoSecs);

/* Tasks moved between CPU queues since boot. */
long GetTaskMigrationCount();

/* Reschedules since boot that found preemption off on the task they landed
   on, and how many of those landed on an idle task (see ScheduleCurrent). */
long GetPreemptDeferredCount();
long GetPreemptDeferredIdleCount();

/* Runnable tasks reported since boot for having waited too long for their
   CPU (TaskQueue::CheckStalls) */
long GetStallReportCount();

/* A task's nice (its share of its CPU, SchedEntity::SetNice) and slice (how
   soon after it can it runs, and for how long at a time), wherever it is:
   under its queue's lock, the queue looked up again if it moved meanwhile.
   From task context. */
void SetTaskNice(Task* task, long nice);
void SetTaskSlice(Task* task, ulong sliceNs);

}