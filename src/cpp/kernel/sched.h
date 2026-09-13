#pragma once

#include "spin_lock.h"
#include "task.h"

#include <lib/stdlib.h>
#include <lib/list_entry.h>

namespace Kernel
{

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

private:
    TaskQueue(const TaskQueue &other) = delete;
    TaskQueue(TaskQueue&& other) = delete;
    TaskQueue& operator=(const TaskQueue& other) = delete;
    TaskQueue& operator=(TaskQueue&& other) = delete;

    Task* SelectNext(Task* curr, bool keepOverIdle);

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
};


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