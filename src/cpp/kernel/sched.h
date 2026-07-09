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

    void Schedule(Task* curr);

    void Clear();

    long GetSwitchContextCounter();

    /* Free tasks that exited on this queue's CPU. Must be called with
       interrupts enabled (it frees stacks, which triggers a blocking TLB
       shootdown) -- see SwitchComplete. */
    void ReapExited();

private:
    TaskQueue(const TaskQueue &other) = delete;
    TaskQueue(TaskQueue&& other) = delete;
    TaskQueue& operator=(const TaskQueue& other) = delete;
    TaskQueue& operator=(TaskQueue&& other) = delete;

    Task* SelectNext(Task* curr);

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
};


void Schedule();
void Sleep(ulong nanoSecs);

}