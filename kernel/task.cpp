#include "task.h"
#include "trace.h"
#include "asm.h"
#include "sched.h"
#include "cpu.h"
#include "sched.h"
#include "preempt.h"
#include <mm/new.h>

namespace Kernel
{

Task::Task()
    : TaskQueue(nullptr)
    , Rsp(0)
    , State(0)
    , Flags(0)
    , Prev(nullptr)
    , Magic(TaskMagic)
    , CpuAffinity(~(0UL))
    , Pid(InvalidObjectId)
    , StackPtr(nullptr)
    , Function(nullptr)
    , Ctx(nullptr)
{
    RefCounter.Set(1);
    ListEntry.Init();
    Name[0] = '\0';
}

Task::Task(const char* fmt, ...)
    : Task()
{
    va_list args;
    va_start(args, fmt);
    Stdlib::VsnPrintf(Name, Stdlib::ArraySize(Name), fmt, args);
    va_end(args);
    Trace(0, "task 0x%p %s", this, Name);
}

Task::~Task()
{
    BugOn(TaskQueue != nullptr);
    BugOn(StackPtr != nullptr);
    Trace(0, "task 0x%p %s dtor", this, Name);
}

void Task::Release()
{
    BugOn(TaskQueue != nullptr);

    if (StackPtr != nullptr)
    {
        Trace(0, "task 0x%p %s free stack 0x%p",
            this, Name, (ulong)StackPtr);
        delete StackPtr;
        StackPtr = nullptr;
    }
}

void Task::Get()
{
    BugOn(RefCounter.Get() <= 0);
    RefCounter.Inc();
}

void Task::Put()
{
    BugOn(RefCounter.Get() <= 0);
    if (RefCounter.DecAndTest())
    {
        Release();
        delete this;
    }
}

void Task::SetStopping()
{
    Flags.SetBit(FlagStoppingBit);
}

bool Task::IsStopping()
{
    return Flags.TestBit(FlagStoppingBit);
}

void Task::Exit()
{
    BugOn(this != GetCurrentTask());

    ExitTime = GetBootTime();
    TaskTable::GetInstance().Remove(this);
    State.Set(StateExited);

    Schedule();

    Panic("Can't be here");
}

void Task::ExecCallback()
{
    Task* curr = GetCurrentTask();
    if (this != curr)
    {
        DiagnoseGetCurrentTask();
        Trace(0, "ExecCallback: this 0x%p GetCurrentTask 0x%p rsp 0x%p",
            (ulong)this, (ulong)curr, GetRsp());
        BugOn(true);
    }
    StartTime = GetBootTime();
    Function(Ctx);
    Exit();
}

void Task::Exec(void *task)
{
    static_cast<Task *>(task)->ExecCallback();
}

void Task::Wait()
{
    while (State.Get() != StateExited)
    {
        Sleep(1 * Const::NanoSecsInMs);
    }
}

bool Task::PrepareStart(Func func, void* ctx)
{
    BugOn(StackPtr != nullptr);
    BugOn(Function != nullptr);

    StackPtr = Mm::TAlloc<Stack, Tag>(this);
    if (StackPtr == nullptr)
    {
        return false;
    }

    Trace(0, "task 0x%p %s stack 0x%p top 0x%p",
        this, Name, (ulong)StackPtr, (ulong)&StackPtr->StackTop[0]);

    if (!TaskTable::GetInstance().Insert(this))
    {
        delete StackPtr;
        StackPtr = nullptr;
        return false;
    }

    Function = func;
    Ctx = ctx;

    return true;
}

bool Task::Start(Func func, void* ctx)
{
    if (!PrepareStart(func, ctx))
        return false;

    ulong* rsp = (ulong *)&StackPtr->StackTop[0];
    *(--rsp) = (ulong)&Task::Exec;//setup return address
    Context* regs = (Context*)((ulong)rsp - sizeof(*regs));
    Stdlib::MemSet(regs, 0, sizeof(*regs));
    regs->Rdi = (ulong)this; //setup 1arg for Task::Exec
    regs->Rflags = (1 << 9); //setup IF
    Rsp = (ulong)regs;

    StartTime = GetBootTime();
    State.Set(StateWaiting);

    auto taskQueue = SelectNextTaskQueue();
    if (taskQueue == nullptr)
        return false;
    taskQueue->Insert(this);
    return true;
}

bool Task::Run(class TaskQueue& taskQueue, Func func, void* ctx)
{
    if (!PrepareStart(func, ctx))
        return false;

    SetRsp((ulong)&StackPtr->StackTop[0]);

    StartTime = GetBootTime();
    RunStartTime = GetBootTime();
    State.Set(StateRunning);

    taskQueue.Insert(this);

    Function(Ctx);

    ExitTime = GetBootTime();

    taskQueue.Remove(this);
    TaskTable::GetInstance().Remove(this);

    return true;
}

Task* Task::GetCurrentTask()
{
    /* Derive the task from RSP by rounding down to StackSize boundary.
       Returns nullptr if the stack doesn't look valid (e.g. during SwitchComplete
       before the new stack is fully set up, or on a non-task stack).
       NOTE: no Trace() calls here — Trace takes a lock which calls
       PreemptDisable -> GetCurrentTask, causing infinite recursion. */
    ulong rsp = GetRsp();
    struct Stack* stackPtr = reinterpret_cast<struct Stack *>(rsp & (~(StackSize - 1)));
    if (BugOn(stackPtr->Magic1 != StackMagic1))
        return nullptr;

    if (BugOn(stackPtr->Magic2 != StackMagic2))
        return nullptr;

    if (BugOn(rsp < ((ulong)&stackPtr->StackBottom[0] + Const::PageSize)))
        return nullptr;

    if (BugOn(rsp > (ulong)&stackPtr->StackTop[0]))
        return nullptr;

    Task* task = stackPtr->Task;
    if (BugOn(task->Magic != TaskMagic))
        return nullptr;

    return task;
}

Task* Task::TryGetCurrentTask()
{
    /*
     * Same as GetCurrentTask but without BugOn — returns nullptr
     * on any invalid state. Safe to call from panic context.
     */
    ulong rsp = GetRsp();
    struct Stack* stackPtr = reinterpret_cast<struct Stack *>(rsp & (~(StackSize - 1)));
    if (stackPtr->Magic1 != StackMagic1 || stackPtr->Magic2 != StackMagic2)
        return nullptr;

    if (rsp < ((ulong)&stackPtr->StackBottom[0] + Const::PageSize))
        return nullptr;

    if (rsp > (ulong)&stackPtr->StackTop[0])
        return nullptr;

    Task* task = stackPtr->Task;
    if (task == nullptr || task->Magic != TaskMagic)
        return nullptr;

    return task;
}

void Task::DiagnoseGetCurrentTask()
{
    /* Safe to call from contexts where Trace is allowed (e.g. Schedule).
       Re-checks the same conditions as GetCurrentTask but logs the failure. */
    ulong rsp = GetRsp();
    struct Stack* stackPtr = reinterpret_cast<struct Stack *>(rsp & (~(StackSize - 1)));

    Trace(0, "DiagTask: rsp 0x%p base 0x%p", rsp, (ulong)stackPtr);
    Trace(0, "DiagTask: Magic1 0x%p expect 0x%p", stackPtr->Magic1, StackMagic1);
    Trace(0, "DiagTask: Magic2 0x%p expect 0x%p", stackPtr->Magic2, StackMagic2);
    Trace(0, "DiagTask: StackBottom+page 0x%p StackTop 0x%p",
        (ulong)&stackPtr->StackBottom[0] + Const::PageSize,
        (ulong)&stackPtr->StackTop[0]);

    if (stackPtr->Magic1 == StackMagic1 && stackPtr->Magic2 == StackMagic2)
    {
        Task* task = stackPtr->Task;
        Trace(0, "DiagTask: Task 0x%p TaskMagic 0x%p expect 0x%p",
            (ulong)task, task->Magic, TaskMagic);
    }
}

void Task::SetName(const char *fmt, ...)
{
    Name[0] = '\0';

    va_list args;
    va_start(args, fmt);
    Stdlib::VsnPrintf(Name, Stdlib::ArraySize(Name), fmt, args);
    va_end(args);
}

const char* Task::GetName()
{
    return Name;
}

void Task::UpdateRuntime()
{
    auto now = GetBootTime();
    Runtime += (now - RunStartTime);
    RunStartTime = now;
}

void Task::SetCpuAffinity(ulong affinity)
{
    Stdlib::AutoLock lock(Lock);
    CpuAffinity = affinity;
}

ulong Task::GetCpuAffinity()
{
    Stdlib::AutoLock lock(Lock);
    return CpuAffinity;
}

TaskQueue* Task::SelectNextTaskQueue()
{
    class TaskQueue* taskQueue = nullptr;
    ulong cpuMask = CpuTable::GetInstance().GetRunningCpus() & CpuAffinity;
    if (cpuMask != 0)
    {
        for (ulong i = 0; i < 8 * sizeof(ulong); i++)
        {
            if (cpuMask & (1UL << i))
            {
                auto& candTaskQueue = CpuTable::GetInstance().GetCpu(i).GetTaskQueue();
                if (&candTaskQueue == TaskQueue)
                    continue;

                if (taskQueue == nullptr)
                {
                    taskQueue = &candTaskQueue;
                }
                else
                {
                    if (taskQueue->GetSwitchContextCounter() > candTaskQueue.GetSwitchContextCounter())
                    {
                        taskQueue = &candTaskQueue;
                    }
                }

                continue;
            }
        }
    }

    return taskQueue;
}

TaskTable::TaskTable()
{
}

TaskTable::~TaskTable()
{
    for (size_t i = 0; i < Stdlib::ArraySize(TaskList); i++)
    {
        Stdlib::ListEntry taskList;

        {
            Stdlib::AutoLock lock(Lock[i]);
            taskList.MoveTailList(&TaskList[i]);
        }

        while (!taskList.IsEmpty())
        {
            Task* task = CONTAINING_RECORD(taskList.RemoveHead(), Task, TableListEntry);
            task->TableListEntry.Init();
            task->Put();
        }
    }
}

bool TaskTable::Insert(Task *task)
{
    ObjectId pid = TaskObjectTable.Insert(task);
    if (pid == InvalidObjectId)
    {
        return false;
    }
    task->Pid = pid;

    task->Get();
    size_t i = Stdlib::HashPtr(task) % Stdlib::ArraySize(TaskList);
    Stdlib::AutoLock lock(Lock[i]);

    BugOn(!task->TableListEntry.IsEmpty());
    TaskList[i].InsertTail(&task->TableListEntry);

    return true;
}

void TaskTable::Remove(Task *task)
{
    {
        size_t i = Stdlib::HashPtr(task) % Stdlib::ArraySize(TaskList);

        TaskObjectTable.Remove(task->Pid);

        Stdlib::AutoLock lock(Lock[i]);

        BugOn(task->TableListEntry.IsEmpty());
        task->TableListEntry.RemoveInit();
    }

    task->Put();
}

Task* TaskTable::Lookup(ulong pid)
{
    return reinterpret_cast<Task*>(TaskObjectTable.Lookup(pid));
}

void TaskTable::Ps(Stdlib::Printer& printer)
{
    printer.Printf("pid state flags runtime ctxswitches name\n");

    for (size_t i = 0; i < Stdlib::ArraySize(TaskList); i++)
    {
        Stdlib::AutoLock lock(Lock[i]);

        for (auto currEntry = TaskList[i].Flink;
            currEntry != &TaskList[i];
            currEntry = currEntry->Flink)
        {
            Task* task = CONTAINING_RECORD(currEntry, Task, TableListEntry);
            printer.Printf("%u %u 0x%p %u.%u %u %s\n",
                task->Pid, task->State.Get(), task->Flags.Get(), task->Runtime.GetSecs(),
                task->Runtime.GetUsecs(), task->ContextSwitches.Get(), task->GetName());
        }
    }
}

}