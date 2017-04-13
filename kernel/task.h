#pragma once

#include <lib/stdlib.h>
#include <lib/list_entry.h>

#include "atomic.h"
#include "forward.h"
#include "spin_lock.h"

namespace Kernel
{

namespace Core
{

class Task final
{
public:

    static const ulong StackSize = 8 * Shared::PageSize;
    static const ulong StackMagic1 = 0xBCDEBCDE;
    static const ulong StackMagic2 = 0xCBDECBDE;

    struct Stack
    {
        Stack(Task* task)
            : Task(task)
            , Magic1(StackMagic1)
            , Magic2(StackMagic2)
        {
        }

        Task* Task;
        ulong Magic1;
        u8 StackBottom[StackSize - 3 * sizeof(ulong)];
        u8 StackTop[0];
        ulong Magic2;
    } __attribute__((packed));

    static_assert(sizeof(Stack) == StackSize, "Invalid size");

    using Func = void (*)(void *ctx);

    Task();
    ~Task();

    bool Run(Func func, void* ctx);

    static Task* GetCurrentTask();

    void Get();
    void Put();

    bool Start(Func func, void* ctx);

public:
    Shared::ListEntry ListEntry;
    TaskQueue* TaskQueue;
    SpinLock Lock;
    Atomic PreemptCounter;
    ulong Rsp;

private:
    Task(const Task& other) = delete;
    Task(Task&& other) = delete;
    Task& operator=(const Task& other) = delete;
    Task& operator=(Task&& other) = delete;

    void Release();

    Stack* Stack;
    Func Function;
    void* Ctx;
    Atomic RefCounter;
};

}
}