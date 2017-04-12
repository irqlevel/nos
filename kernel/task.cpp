#include "task.h"
#include "trace.h"
#include "asm.h"

namespace Kernel
{

namespace Core
{

Task::Task()
    : Stack(nullptr)
    , Function(nullptr)
    , Ctx(nullptr)
{
}

Task::~Task()
{
    if (Stack != nullptr)
    {
        delete Stack;
    }
}

bool Task::Run(Func func, void* ctx)
{
    Stack = new struct Stack(this);
    if (Stack == nullptr)
    {
        return false;
    }

    SwitchRsp((ulong)&Stack->StackTop[0]);
    Function = func;
    Ctx = ctx;

    Function(Ctx);
    return true;
}

}
}