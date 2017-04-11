#include "task.h"
#include "trace.h"

namespace Kernel
{

namespace Core
{

Task::Task(TaskRountinePtr routine, void* ctx)
    : Routine(routine)
    , Ctx(ctx)
    , TaskInfoPtr(nullptr)
{
    TaskInfoPtr = new TaskInfo(this);
}

void Task::Run()
{
    Routine(Ctx);
}

Task::~Task()
{
    if (TaskInfoPtr != nullptr)
    {
        delete TaskInfoPtr;
        TaskInfoPtr = nullptr;
    }
}

}
}