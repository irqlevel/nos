#include "task.h"
#include "trace.h"
#include "asm.h"
#include "sched.h"
#include "cpu.h"

namespace Kernel
{

Task::Task()
    : TaskQueue(nullptr)
    , Rsp(0)
    , Stack(nullptr)
    , Function(nullptr)
    , Ctx(nullptr)
{
    RefCounter.Set(1);
    ListEntry.Init();
    PreemptDisable.Set(0);
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

void Task::ExecCallback()
{
    Function(Ctx);
}

void Task::Exec(void *task)
{
    static_cast<Task *>(task)->ExecCallback();
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

    auto& cpu = CpuTable::GetInstance().GetCurrentCpu();
    cpu.GetTaskQueue().AddTask(this);
    return true;
}

bool Task::Run(Func func, void* ctx)
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

    Function(Ctx);
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

}