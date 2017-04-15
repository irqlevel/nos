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

    if (curr->State.Get() != Task::StateExited)
        curr->State.Set(Task::StateWaiting);
    curr->ContextSwitches.Inc();

    BugOn(next->State.Get() == Task::StateExited);
    next->State.Set(Task::StateRunning);

    SwitchContext(next->Rsp, &curr->Rsp);
}

void TaskQueue::Schedule()
{
    ScheduleCounter.Inc();

    if (unlikely(!PreemptIsOn()))
        return;

    Task* curr = Task::GetCurrentTask();
    if (curr->PreemptDisableCounter.Get() != 0)
        return;

    Task* next = nullptr;
    {
        Shared::AutoLock lock(Lock);
        Shared::AutoLock lock2(curr->Lock);

        BugOn(TaskList.IsEmpty());

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

            if (cand->PreemptDisableCounter.Get() != 0)
                continue;

            next = cand;
            break;
        }

        if (next == nullptr)
            return;

        Shared::AutoLock lock3(next->Lock);
        next->ListEntry.Remove();
        if (curr->TaskQueue != nullptr)
        {
            BugOn(curr->TaskQueue != this);
            curr->ListEntry.Remove();
            TaskList.InsertTail(&curr->ListEntry);
        }
        TaskList.InsertTail(&next->ListEntry);
    }

    Switch(next, curr);
}

void TaskQueue::Insert(Task* task)
{
    task->Get();

    Shared::AutoLock lock(Lock);
    Shared::AutoLock lock2(task->Lock);

    BugOn(task->TaskQueue != nullptr);
    BugOn(!(task->ListEntry.IsEmpty()));

    task->TaskQueue = this;
    TaskList.InsertTail(&task->ListEntry);
}

void TaskQueue::Remove(Task* task)
{
    {
        Shared::AutoLock lock(Lock);
        Shared::AutoLock lock2(task->Lock);

        BugOn(task->TaskQueue != this);
        BugOn(task->ListEntry.IsEmpty());
        task->TaskQueue = nullptr;
        task->ListEntry.RemoveInit();
    }

    task->Put();
}

TaskQueue::~TaskQueue()
{
    Shared::ListEntry taskList;
    {
        Shared::AutoLock lock(Lock);
        taskList.MoveTailList(&TaskList);
    }

    while (!taskList.IsEmpty())
    {
        Task* task = CONTAINING_RECORD(taskList.RemoveHead(), Task, ListEntry);
        Shared::AutoLock lock2(task->Lock);
        BugOn(task->TaskQueue != this);
        task->TaskQueue = nullptr;
        task->Put();
    }

    Trace(0, "TaskQueue 0x%p counters: sched %u switch context %u",
        this, ScheduleCounter.Get(), SwitchContextCounter.Get());

}

}