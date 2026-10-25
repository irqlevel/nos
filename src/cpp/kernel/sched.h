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

    /* One writer, so a load and a store and no locked instruction: what a
       reader on another CPU sees is the value before or the value after */
    static void Bump(Atomic& counter, ulong by);
    static void Raise(Atomic& max, ulong value);

    Atomic WakeHist[Buckets];
    Atomic WakeMax;
    Atomic DelayHist[Buckets];
    Atomic Decisions;
    Atomic DecisionCycles;
    Atomic DecisionMax;
    Atomic Walked;
    Atomic WalkMax;
    Atomic Looks;
    Atomic RunnableSum;
    Atomic RunnableMax;
    Atomic Crowded;
};

class TaskQueue
{
public:
    TaskQueue();
    ~TaskQueue();

    void Insert(Task* task);
    void Remove(Task* task);

    /* keepOverIdle: curr keeps the CPU rather than hand it to the idle
       task, when the idle task is all there is (YieldToRunnable, Preempt) */
    void Schedule(Task* curr, bool keepOverIdle = false);

    void Clear();

    long GetSwitchContextCounter();

    /* How many tasks sit on this queue. Maintained by every membership
       change, which is not the same set as every list operation: SelectNext
       rotates the list without changing what is on it. */
    long GetTaskCount();

    /* Free tasks that exited on this queue's CPU. Must be called with
       interrupts enabled (it frees stacks, which triggers a blocking TLB
       shootdown) -- see SwitchComplete. */
    void ReapExited();

    void ReadStats(SchedStats::Snapshot& out);

private:
    TaskQueue(const TaskQueue &other) = delete;
    TaskQueue(TaskQueue&& other) = delete;
    TaskQueue& operator=(const TaskQueue& other) = delete;
    TaskQueue& operator=(TaskQueue&& other) = delete;

    /* walked counts the entries looked at, for SchedStats */
    Task* SelectNext(Task* curr, bool keepOverIdle, ulong& walked);

    /* The soonest deadline after now of a task blocked on this queue until
       one (Sleep, Event::WaitFor) -- what this CPU's timer is to fire for
       before its next tick -- or ~0 when there is none. Under Lock. */
    ulong EarliestDeadline(ulong now);

    /* This CPU's timer brought forward to what is asleep here: after it
       fired, to the soonest deadline of all (EarliestDeadline); otherwise to
       curr's, blocking now. Under Lock, interrupts off. */
    void WakeSleepers(Task* curr);

    /* Spins allowed in Schedule while an exited task waits for a
       runnable candidate before declaring the queue broken */
    static const ulong MaxExitedRetries = 100000000;

    void Switch(Task* next, Task* curr);

    void SwitchComplete(Task* curr);

    static void SwitchComplete(void* ctx);

    using ListEntry = Stdlib::ListEntry;
    ListEntry TaskList;
    SpinLock Lock;

    /* Tasks that exited on this CPU, pending a stack free with interrupts
       enabled (ReapExited), rather than in the IRQs-off switch path. */
    ListEntry ExitedList;
    SpinLock ExitedLock;

    Atomic ScheduleCounter;
    Atomic SwitchContextCounter;
    Atomic TaskCount;

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

}