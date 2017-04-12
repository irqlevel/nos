#include "spin_lock.h"
#include "task.h"

#include <lib/stdlib.h>
#include <lib/list_entry.h>

namespace Kernel
{

namespace Core
{

class TaskQueue
{
public:
    TaskQueue();
    ~TaskQueue();

    void Schedule();

private:
    TaskQueue(const TaskQueue &other) = delete;
    TaskQueue(TaskQueue&& other) = delete;
    TaskQueue& operator=(const TaskQueue& other) = delete;
    TaskQueue& operator=(TaskQueue&& other) = delete;

    using ListEntry = Shared::ListEntry;
    ListEntry TaskList;
    SpinLock Lock;
};

}

}