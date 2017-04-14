#include "task.h"
#include "trace.h"
#include "asm.h"
#include "sched.h"
#include "cpu.h"
#include "sched.h"

namespace Kernel
{

Task::Task()
    : TaskQueue(nullptr)
    , Rsp(0)
    , State(0)
    , Stack(nullptr)
    , Function(nullptr)
    , Ctx(nullptr)
{
    RefCounter.Set(1);
    ListEntry.Init();
    PreemptDisable.Set(0);
    Name[0] = '\0';
}

Task::Task(const char* fmt, ...)
    : Task()
{
    va_list args;
    va_start(args, fmt);
    Shared::VsnPrintf(Name, Shared::ArraySize(Name), fmt, args);
    va_end(args);
}

Task::~Task()
{
    Put();
    BugOn(TaskQueue != nullptr);
    BugOn(Stack != nullptr);
}

void Task::Release()
{
    BugOn(TaskQueue != nullptr);

    if (Stack != nullptr)
    {
        delete Stack;
        Stack = nullptr;
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
    }
}

void Task::SetStopping()
{
    State |= StateStopping;
}

bool Task::IsStopping()
{
    return (State & StateStopping) ? true : false;
}

void Task::Exit()
{
    class TaskQueue *tq = TaskQueue;

    BugOn(tq == nullptr);

    tq->Remove(this);
    State |= StateExited;
    TaskTable::GetInstance().Remove(this);
    tq->Schedule();
    Panic("Can't be here");
}

void Task::ExecCallback()
{
    Function(Ctx);
    Exit();
}

void Task::Exec(void *task)
{
    static_cast<Task *>(task)->ExecCallback();
}

void Task::Wait()
{
    while (!(State & StateExited))
    {
        GetCpu().Sleep(1000000);
    }
}

bool Task::Start(Func func, void* ctx)
{
    Stack = new struct Stack(this);
    if (Stack == nullptr)
    {
        return false;
    }

    Function = func;
    Ctx = ctx;

    ulong* rsp = (ulong *)&Stack->StackTop[0];
    *(--rsp) = (ulong)&Task::Exec;//setup return address
    Context* regs = (Context*)((ulong)rsp - sizeof(*regs));
    Shared::MemSet(regs, 0, sizeof(*regs));
    regs->Rdi = (ulong)this; //setup 1arg for Task::Exec
    regs->Rflags = (1 << 9); //setup IF
    Rsp = (ulong)regs;

    TaskTable::GetInstance().Insert(this);
    auto& cpu = CpuTable::GetInstance().GetCurrentCpu();
    cpu.GetTaskQueue().Insert(this);
    return true;
}

bool Task::Run(class TaskQueue& taskQueue, Func func, void* ctx)
{
    BugOn(Stack != nullptr);
    BugOn(Function != nullptr);

    Stack = new struct Stack(this);
    if (Stack == nullptr)
    {
        return false;
    }

    SetRsp((ulong)&Stack->StackTop[0]);
    Function = func;
    Ctx = ctx;

    TaskTable::GetInstance().Insert(this);
    taskQueue.Insert(this);
    Function(Ctx);
    taskQueue.Remove(this);
    TaskTable::GetInstance().Remove(this);

    return true;
}

Task* Task::GetCurrentTask()
{
    struct Stack* stack = reinterpret_cast<struct Stack *>(GetRsp() & (~(StackSize - 1)));
    if (BugOn(stack->Magic1 != StackMagic1))
        return nullptr;

    if (BugOn(stack->Magic2 != StackMagic2))
        return nullptr;

    return stack->Task;
}

void Task::SetName(const char *fmt, ...)
{
    Name[0] = '\0';

    va_list args;
    va_start(args, fmt);
    Shared::VsnPrintf(Name, Shared::ArraySize(Name), fmt, args);
    va_end(args);
}

const char* Task::GetName()
{
    return Name;
}

TaskTable::TaskTable()
{
    TaskList.Init();
}

TaskTable::~TaskTable()
{
    Shared::ListEntry taskList;

    {
        Shared::AutoLock lock(Lock);
        taskList.MoveTailList(&TaskList);
    }

    while (!taskList.IsEmpty())
    {
        Task* task = CONTAINING_RECORD(taskList.RemoveHead(), Task, TableListEntry);
        task->TableListEntry.Init();
        task->Put();
    }
}

void TaskTable::Insert(Task *task)
{
    task->Get();

    Shared::AutoLock lock(Lock);

    BugOn(!task->TableListEntry.IsEmpty());
    TaskList.InsertTail(&task->TableListEntry);
}

void TaskTable::Remove(Task *task)
{
    {
        Shared::AutoLock lock(Lock);

        BugOn(task->TableListEntry.IsEmpty());
        task->TableListEntry.RemoveInit();
    }

    task->Put();
}

void TaskTable::Ps(Shared::Printer& printer)
{
    Shared::AutoLock lock(Lock);

    for (auto currEntry = TaskList.Flink;
        currEntry != &TaskList;
        currEntry = currEntry->Flink)
    {
        Task* task = CONTAINING_RECORD(currEntry, Task, TableListEntry);
        printer.Printf("0x%p 0x%p %u %s\n",
            task, task->State, task->ContextSwitches.Get(), task->GetName());
    }
}

}