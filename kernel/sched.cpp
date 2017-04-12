#include "sched.h"

namespace Kernel
{

namespace Core
{

TaskQueue::TaskQueue()
{
    TaskList.Init();
}

void TaskQueue::Schedule()
{
}

TaskQueue::~TaskQueue()
{
}

}
}