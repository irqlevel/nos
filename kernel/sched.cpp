#include "sched.h"
#include "panic.h"
#include "trace.h"
#include "preempt.h"

namespace Kernel
{

namespace Core
{

TaskQueue::TaskQueue()
{
    TaskList.Init();
}

void TaskQueue::Switch(Task* next, Task* curr)
{
    BugOn(curr == next);

    Trace(0, "Switch task 0x%p -> 0x%p", curr, next);

    SwitchContext(next->Rsp, &curr->Rsp);
}

void TaskQueue::Schedule()
{
    if (!PreemptIsActive())
    {
        return;
    }

    Shared::AutoLock lock(Lock);
    Task* curr = Task::GetCurrentTask();
    Shared::AutoLock lock2(curr->Lock);
    if (curr->TaskQueue == nullptr)
        return;

    BugOn(curr->TaskQueue != this);
    BugOn(TaskList.IsEmpty());

    Task* next = CONTAINING_RECORD(TaskList.RemoveHead(), Task, ListEntry);
    if (next == curr)
    {
        TaskList.InsertTail(&next->ListEntry);
        return;
    }

    Shared::AutoLock lock3(next->Lock);

    curr->ListEntry.Remove();
    TaskList.InsertTail(&curr->ListEntry);
    TaskList.InsertTail(&next->ListEntry);
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
}

}
}