#pragma once

#include "stdlib.h"
#include "list_entry.h"

namespace Kernel
{

namespace Core
{

const ulong TaskInfoSize = 4 * Shared::PageSize;

class Task;

struct TaskInfo
{
    TaskInfo(Task* task)
        : TaskPtr(task)
    {
    }

    Task* TaskPtr;
    u8 StackBottom[TaskInfoSize - sizeof(ulong)];
} __attribute__((packed));

static_assert(sizeof(TaskInfo) == TaskInfoSize, "Invalid size");

using TaskRountinePtr = void (*)(void *ctx);

class Task final
{
public:
    Task(TaskRountinePtr routine, void* ctx);
    ~Task();

    Shared::ListEntry List;

    void Run();

private:
    Task(const Task& other) = delete;
    Task(Task&& other) = delete;
    Task& operator=(const Task& other) = delete;
    Task& operator=(Task&& other) = delete;

    TaskRountinePtr Routine;
    void* Ctx;
    TaskInfo* TaskInfoPtr;
};

}
}