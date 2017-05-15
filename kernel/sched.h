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

private:
    TaskQueue(const TaskQueue &other) = delete;
    TaskQueue(TaskQueue&& other) = delete;
    TaskQueue& operator=(const TaskQueue& other) = delete;
    TaskQueue& operator=(TaskQueue&& other) = delete;

    void Switch(Task* curr, Task* next);

    void SwitchComplete(Task* curr);

    static void SwitchComplete(void* ctx);

    using ListEntry = Shared::ListEntry;
    ListEntry TaskList;
    SpinLock Lock;

    Atomic ScheduleCounter;
    Atomic SwitchContextCounter;
};


void Schedule();
void Sleep(ulong nanoSecs);

}