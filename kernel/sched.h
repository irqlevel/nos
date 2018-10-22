#pragma once

#include "spin_lock.h"
#include "task.h"

#include <lib/stdlib.h>
#include <lib/list_entry.h>

namespace Kernel
{

class Cpu;
class TaskQueue
{
public:
    TaskQueue(Cpu* owner);
    ~TaskQueue();

    void Insert(Task* task);
    void Remove(Task* task);

    void Schedule(Task* curr);

    void Clear();

    long GetSwitchContextCounter();

    Cpu* GetCpu();

private:
    TaskQueue(const TaskQueue &other) = delete;
    TaskQueue(TaskQueue&& other) = delete;
    TaskQueue& operator=(const TaskQueue& other) = delete;
    TaskQueue& operator=(TaskQueue&& other) = delete;

    Task* SelectNext(Task* curr);

    void Switch(Task* curr, Task* next);

    void SwitchComplete(Task* curr);

    static void SwitchComplete(void* ctx);

    using ListEntry = Stdlib::ListEntry;
    ListEntry TaskList;
    SpinLock Lock;

    Atomic ScheduleCounter;
    Atomic SwitchContextCounter;
    Cpu* Owner;
};


void Schedule();
void Sleep(ulong nanoSecs);

}