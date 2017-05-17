#pragma once

#include <lib/stdlib.h>
#include <lib/list_entry.h>
#include <lib/printer.h>

#include "atomic.h"
#include "forward.h"
#include "spin_lock.h"
#include "panic.h"

namespace Kernel
{

class Task final
{
public:

    static const ulong StackSize = 8 * Shared::PageSize;
    static const ulong StackMagic1 = 0xBCDEBCDE;
    static const ulong StackMagic2 = 0xCBDECBDE;

    static const ulong TaskMagic = 0xCBDECBEF;

    struct Stack final
    {
        Stack(Task* task)
            : Task(task)
            , Magic1(StackMagic1)
            , Magic2(StackMagic2)
        {
        }

        ~Stack()
        {
            BugOn(Magic1 != StackMagic1);
            BugOn(Magic2 != StackMagic2);
            Magic1 = 0;
            Magic2 = 0;
            Task = nullptr;
        }

        Task* Task;
        ulong Magic1;
        u8 StackBottom[StackSize - 3 * sizeof(ulong)];
        u8 StackTop[0];
        ulong Magic2;

    private:
        Stack(const Stack& other) = delete;
        Stack(Stack&& other) = delete;
        Stack& operator=(const Stack& other) = delete;
        Stack& operator=(Stack&& other) = delete;
    } __attribute__((packed));

    static_assert(sizeof(Stack) == StackSize, "Invalid size");

    using Func = void (*)(void *ctx);

    Task();
    Task(const char* fmt, ...);

    bool Run(class TaskQueue& taskQueue, Func func, void* ctx);

    static Task* GetCurrentTask();

    void Get();
    void Put();

    bool Start(Func func, void* ctx);

    void Wait();

    void SetStopping();
    bool IsStopping();

    void SetName(const char *fmt, ...);
    const char* GetName();

    void UpdateRuntime();

    void SetCpuAffinity(ulong affinity);
    ulong GetCpuAffinity();

    TaskQueue* SelectNextTaskQueue();

    static const long StateWaiting = 1;
    static const long StateRunning = 2;
    static const long StateExited = 3;

    static const long FlagStoppingBit = 1;

public:
    Shared::ListEntry ListEntry;
    Shared::ListEntry TableListEntry;

    TaskQueue* TaskQueue;
    SpinLock Lock;
    Atomic PreemptDisableCounter;
    Atomic ContextSwitches;
    ulong Rsp;

    Atomic State;
    Atomic Flags;

    Shared::Time RunStartTime;
    Shared::Time Runtime;
    Shared::Time StartTime;
    Shared::Time ExitTime;

    Task* Prev;
    ulong Magic;
    ulong CpuAffinity;

private:
    Task(const Task& other) = delete;
    Task(Task&& other) = delete;
    Task& operator=(const Task& other) = delete;
    Task& operator=(Task&& other) = delete;
    ~Task();

    void Release();
    void Exit();
    void ExecCallback();
    static void Exec(void *task);

    Stack* Stack;
    Func Function;
    void* Ctx;
    Atomic RefCounter;

    char Name[32];
};

class TaskTable final
{
public:
    static TaskTable& GetInstance()
    {
        static TaskTable Instance;
        return Instance;
    }

    void Insert(Task *task);
    void Remove(Task *task);

    void Ps(Shared::Printer& printer);

private:
    TaskTable(const TaskTable& other) = delete;
    TaskTable(TaskTable&& other) = delete;
    TaskTable& operator=(const TaskTable& other) = delete;
    TaskTable& operator=(TaskTable&& other) = delete;

    TaskTable();
    ~TaskTable();

    static const size_t TaskListCount = 512;

    SpinLock Lock[TaskListCount];
    Shared::ListEntry TaskList[TaskListCount];
};

}