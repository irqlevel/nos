#pragma once

#include <lib/stdlib.h>
#include <lib/list_entry.h>

namespace Kernel
{

namespace Core
{

class Task final
{
public:

    static const ulong StackSize = Shared::PageSize;

    struct Stack
    {
        Stack(Task* task)
            : Task(task)
        {
        }

        Task* Task;
        u8 StackBottom[StackSize - sizeof(ulong)];
        u8 StackTop[0];
    } __attribute__((packed));

    static_assert(sizeof(Stack) == StackSize, "Invalid size");

    using Func = void (*)(void *ctx);

    Task();
    ~Task();

    bool Run(Func func, void* ctx);

public:
    Shared::ListEntry List;

private:
    Task(const Task& other) = delete;
    Task(Task&& other) = delete;
    Task& operator=(const Task& other) = delete;
    Task& operator=(Task&& other) = delete;

    Stack* Stack;
    Func Function;
    void* Ctx;
};

}
}