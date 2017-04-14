#include "sched.h"
#include "panic.h"
#include "trace.h"
#include "preempt.h"

namespace Kernel
{

TaskQueue::TaskQueue()
{
    Shared::AutoLock lock(Lock);
    TaskList.Init();

    SwitchContextCounter.Set(0);
    ScheduleCounter.Set(0);
}

void TaskQueue::Switch(Task* next, Task* curr)
{
    SwitchContextCounter.Inc();

    BugOn(curr == next);
    BugOn(next->Rsp == 0);

    SwitchContext(next->Rsp, &curr->Rsp);
}

void TaskQueue::Schedule()
{
    ScheduleCounter.Inc();

    if (!PreemptIsOn())
        return;

    Task* curr = Task::GetCurrentTask();
    if (curr->PreemptDisable.Get() != 0)
        return;

    Task* next;
    {
        Shared::AutoLock lock(Lock);
        Shared::AutoLock lock2(curr->Lock);

        BugOn(curr->TaskQueue != this);
        BugOn(TaskList.IsEmpty());

        next = nullptr;
        for (auto currEntry = TaskList.Flink;
            currEntry != &TaskList;
            currEntry = currEntry->Flink)
        {
            Task* cand = CONTAINING_RECORD(currEntry, Task, ListEntry);
            if (cand == curr)
            {
                cand->ListEntry.Remove();
                TaskList.InsertTail(&cand->ListEntry);
                return;
            }

            if (cand->PreemptDisable.Get() != 0)
                continue;

            next = cand;
            break;
        }

        if (next == nullptr)
            return;

        Shared::AutoLock lock3(next->Lock);
        next->ListEntry.Remove();
        curr->ListEntry.Remove();
        TaskList.InsertTail(&curr->ListEntry);
        TaskList.InsertTail(&next->ListEntry);
    }

    Switch(next, curr);
}

void TaskQueue::AddTask(Task* task)
{
    task->Get();

    Shared::AutoLock lock(Lock);
    Shared::AutoLock lock2(task->Lock);

    BugOn(task->TaskQueue != nullptr);
    BugOn(!(task->ListEntry.IsEmpty()));

    task->TaskQueue = this;
    TaskList.InsertTail(&task->ListEntry);
}

void TaskQueue::RemoveTask(Task* task)
{
    {
        Shared::AutoLock lock(Lock);
        Shared::AutoLock lock2(task->Lock);

        BugOn(task->TaskQueue != this);
        BugOn(task->ListEntry.IsEmpty());
        task->TaskQueue = nullptr;
        task->ListEntry.Remove();
    }

    task->Put();
}

TaskQueue::~TaskQueue()
{
    Shared::AutoLock lock(Lock);

    while (!TaskList.IsEmpty())
    {
        Task* task = CONTAINING_RECORD(TaskList.RemoveHead(), Task, ListEntry);
        Shared::AutoLock lock2(task->Lock);
        BugOn(task->TaskQueue != this);
        task->TaskQueue = nullptr;
        task->Put();
    }

    Trace(0, "TaskQueue 0x%p counters: sched %u switch context %u",
        this, ScheduleCounter.Get(), SwitchContextCounter.Get());

}

}