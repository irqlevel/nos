#include "sched.h"
#include "panic.h"
#include "trace.h"
#include "preempt.h"
#include "time.h"

namespace Kernel
{

TaskQueue::TaskQueue()
{
    Shared::AutoLock lock(Lock);
    TaskList.Init();

    SwitchContextCounter.Set(0);
    ScheduleCounter.Set(0);
}

void TaskQueue::Unlock()
{
    Lock.Unlock();
    InterruptEnable();
}

void TaskQueue::Unlock(void* ctx)
{
    static_cast<TaskQueue*>(ctx)->Unlock();
}

void TaskQueue::Switch(Task* next, Task* curr)
{
    SwitchContextCounter.Inc();

    BugOn(curr == next);
    BugOn(next->Rsp == 0);

    if (curr->State.Get() != Task::StateExited)
        curr->State.Set(Task::StateWaiting);
    curr->ContextSwitches.Inc();
    auto now = GetBootTime();
    curr->Runtime += (now - curr->RunStartTime);

    BugOn(next->State.Get() == Task::StateExited);
    next->State.Set(Task::StateRunning);
    next->RunStartTime = now;

    SwitchContext(next->Rsp, &curr->Rsp, &TaskQueue::Unlock, this);
}

void TaskQueue::Schedule()
{
    BugOn(!IsInterruptEnabled());

    ScheduleCounter.Inc();

    if (unlikely(!PreemptIsOn()))
    {
        return;
    }

    Task *curr = Task::GetCurrentTask();
    if (curr->PreemptDisableCounter.Get() != 0)
    {
        return;
    }

    InterruptDisable();
    Lock.Lock();

    Task* next = nullptr;
    do {
        Shared::AutoLock lock2(curr->Lock);

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

        Shared::AutoLock lock3(next->Lock);
        next->ListEntry.Remove();
        if (curr->TaskQueue != nullptr)
        {
            BugOn(curr->TaskQueue != this);
            curr->ListEntry.Remove();
            TaskList.InsertTail(&curr->ListEntry);
        }
        TaskList.InsertTail(&next->ListEntry);

    } while (false);

    if (next != nullptr)
    {
        Switch(next, curr);
        return;
    }

    Unlock();
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