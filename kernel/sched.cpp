#include "sched.h"
#include "panic.h"
#include "trace.h"
#include "preempt.h"
#include "time.h"
#include "asm.h"
#include "debug.h"

namespace Kernel
{

TaskQueue::TaskQueue()
{
    Shared::AutoLock lock(Lock);
    TaskList.Init();

    SwitchContextCounter.Set(0);
    ScheduleCounter.Set(0);
}

void TaskQueue::SwitchComplete(Task* curr)
{
    Task* prev = curr->Prev;

    curr->Prev = nullptr;
    curr->Lock.Unlock();
    prev->Lock.Unlock();
    Lock.Unlock();

    prev->PreemptDisableCounter.Dec();
}

void TaskQueue::SwitchComplete(void* ctx)
{
    Task* curr = static_cast<Task*>(ctx);
    curr->TaskQueue->SwitchComplete(curr);
}

void TaskQueue::Switch(Task* next, Task* curr)
{
    SwitchContextCounter.Inc();

    BugOn(curr == next);
    BugOn(next->Prev != nullptr);
    BugOn(next->Rsp == 0);

    if (curr->State.Get() != Task::StateExited)
        curr->State.Set(Task::StateWaiting);

    curr->ContextSwitches.Inc();
    curr->UpdateRuntime();

    BugOn(next->State.Get() == Task::StateExited);
    next->State.Set(Task::StateRunning);
    next->RunStartTime = GetBootTime();
    next->Prev = curr;
    SwitchContext(next->Rsp, &curr->Rsp, &TaskQueue::SwitchComplete, next);
}

void TaskQueue::Schedule()
{
    ScheduleCounter.Inc();

    if (unlikely(!PreemptIsOn()))
    {
        return;
    }

    Task *curr = Task::GetCurrentTask();
    if (curr->PreemptDisableCounter.Get() != 0)
    {
        Shared::AutoLock lock(curr->Lock);
        curr->UpdateRuntime();
        return;
    }

    curr->PreemptDisableCounter.Inc();
    ulong flags = GetRflags();
    InterruptDisable();
    Lock.Lock();

    Task* next = nullptr;
    do {
        curr->Lock.Lock();
        BugOn(TaskList.IsEmpty());

        for (auto currEntry = TaskList.Flink;
            currEntry != &TaskList;
            currEntry = currEntry->Flink)
        {
            Task* cand = CONTAINING_RECORD(currEntry, Task, ListEntry);
            if (cand == curr)
            {
                BugOn(cand->State.Get() == Task::StateExited);
                cand->ListEntry.Remove();
                TaskList.InsertTail(&cand->ListEntry);
                break;
            }

            if (cand->PreemptDisableCounter.Get() != 0)
            {
                continue;
            }
            next = cand;
            break;
        }

        if (next == nullptr)
        {
            break;
        }

        next->Lock.Lock();
        next->ListEntry.Remove();
        if (curr->TaskQueue != nullptr)
        {
            BugOn(curr->TaskQueue != this);
            curr->ListEntry.Remove();
            TaskList.InsertTail(&curr->ListEntry);
        }
        TaskList.InsertTail(&next->ListEntry);

    } while (false);

    if (next == nullptr)
    {
        curr->UpdateRuntime();
        curr->Lock.Unlock();
        Lock.Unlock();
        SetRflags(flags);
        curr->PreemptDisableCounter.Dec();
        return;
    }

    Switch(next, curr);
    SetRflags(flags);
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

void TaskQueue::Clear()
{
    Shared::ListEntry taskList;
    {
        Shared::AutoLock lock(Lock);
        taskList.MoveTailList(&TaskList);
    }

    if (taskList.IsEmpty())
        return;

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

TaskQueue::~TaskQueue()
{
    Clear();
}

}