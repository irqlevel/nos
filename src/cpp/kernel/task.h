#pragma once

#include <lib/stdlib.h>
#include <lib/list_entry.h>
#include <lib/printer.h>

#include "atomic.h"
#include "forward.h"
#include "spin_lock.h"
#include "panic.h"
#include "object_table.h"

namespace Kernel
{

/* Task name buffer, shared by Task and by TaskTable::CpuSample. */
static const size_t TaskNameLen = 32;

class Task final : public Object
{
public:

    static const ulong StackSize = 8 * Const::PageSize;
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
    static Task* TryGetCurrentTask();
    static void DiagnoseGetCurrentTask();

    virtual void Get() override;
    virtual void Put() override;

    bool Start(Func func, void* ctx);

    void Wait();

    void SetStopping();
    bool IsStopping();

    void SetName(const char *fmt, ...);
    const char* GetName();

    /* Bytes of this task's stack that the poison scan still finds intact --
       the part it has never reached. 0 when it has no stack yet. */
    ulong GetStackFree();

    void UpdateRuntime();

    void SetCpuAffinity(ulong affinity);
    ulong GetCpuAffinity();

    /* How much lighter another CPU's queue has to be before a running task
       is moved to it. Two, because the task is still counted where it is. */
    static const long MigrateThreshold = 2;

    /* The queue this task should be on: the lightest one its affinity
       allows, or nullptr to say it is already on the right one. */
    void SetIdle();
    bool IsIdle();

    /* A task that has asked not to be scheduled until someone wakes it.
       Block() is called by the task itself, immediately before the Schedule()
       that switches it out; Unblock() may be called from any CPU and from
       hard IRQ context -- both are a single atomic bit operation and take no
       lock. */
    void Block();
    void Unblock();
    bool IsBlocked();

    TaskQueue* SelectNextTaskQueue();

    static const long StateWaiting = 1;
    static const long StateRunning = 2;
    static const long StateExited = 3;

    static const long FlagStoppingBit = 1;

    /* The CPU's idle task. Marked so the scheduler can treat it as the last
       resort it is, rather than as one more task to take a turn. */
    static const long FlagIdleBit = 2;

    /* Set while a task is waiting to be woken. The scheduler passes over it
       entirely -- unlike the idle task, which is merely last. */
    static const long FlagBlockedBit = 3;

public:
    Stdlib::ListEntry ListEntry;
    Stdlib::ListEntry TableListEntry;

    TaskQueue* TaskQueue;
    SpinLock Lock;
    Atomic PreemptDisableCounter;
    Atomic ContextSwitches;
    ulong Rsp;

    Atomic State;
    Atomic Flags;

    Stdlib::Time RunStartTime;
    Stdlib::Time Runtime;
    Stdlib::Time StartTime;
    Stdlib::Time ExitTime;

    Task* Prev;
    ulong Magic;
    ulong CpuAffinity;
    ulong Pid;

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

    bool PrepareStart(Func func, void* ctx);

    Stack* StackPtr;
    Func Function;
    void* Ctx;
    Atomic RefCounter;

    char Name[TaskNameLen];

    static const ulong Tag = 'Task';
};

class TaskTable final
{
public:
    static TaskTable& GetInstance()
    {
        static TaskTable Instance;
        return Instance;
    }

    bool Insert(Task *task);
    void Remove(Task *task);

    Task* Lookup(ulong pid);

    void Ps(Stdlib::Printer& printer);

    /* High-water marks of every task stack, worst free tracked in worstFree. */
    void Stacks(Stdlib::Printer& printer, ulong& worstFree);

    /* One task's accumulated CPU time, copied out under the bucket lock so
       the sample stays readable after the task itself is gone -- `top` takes
       two of these a moment apart and subtracts. */
    struct CpuSample
    {
        ulong Pid;
        ulong RuntimeNs;
        char Name[TaskNameLen];
    };

    size_t SampleCpu(CpuSample* out, size_t max);

private:
    TaskTable(const TaskTable& other) = delete;
    TaskTable(TaskTable&& other) = delete;
    TaskTable& operator=(const TaskTable& other) = delete;
    TaskTable& operator=(TaskTable&& other) = delete;

    TaskTable();
    ~TaskTable();

    static const size_t TaskListCount = 512;

    /* Ps walks each bucket holding that bucket's lock with interrupts off. A
       damaged list -- a cycle, a half-written link -- turns that walk into an
       infinite loop on a CPU nobody can interrupt, holding a lock the
       scheduler wants, and the machine stops without a panic and without a
       reset. There is no legitimate way for one of 512 buckets to hold this
       many tasks, so passing it means the list is broken; say so and stop,
       which leaves a diagnosis instead of a brick. */
    static const size_t MaxTasksPerList = 4096;

    SpinLock Lock[TaskListCount];
    Stdlib::ListEntry TaskList[TaskListCount];

    ObjectTable TaskObjectTable;
};

}