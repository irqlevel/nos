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

    void AddTask(Task* task);
    void RemoveTask(Task* task);

    void Schedule();

private:
    TaskQueue(const TaskQueue &other) = delete;
    TaskQueue(TaskQueue&& other) = delete;
    TaskQueue& operator=(const TaskQueue& other) = delete;
    TaskQueue& operator=(TaskQueue&& other) = delete;

    void Switch(Task* curr, Task* next);

    using ListEntry = Shared::ListEntry;
    ListEntry TaskList;
    SpinLock Lock;

    Atomic ScheduleCounter;
    Atomic SwitchContextCounter;
};

}