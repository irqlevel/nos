#pragma once

#include <lib/stdlib.h>
#include <lib/list_entry.h>
#include <lib/printer.h>

#include "atomic.h"
#include "forward.h"
#include "spin_lock.h"
#include "panic.h"
#include "object_table.h"
#include "eevdf.h"

namespace Kernel
{

/* Task name buffer, shared by Task and by TaskTable::CpuSample. */
static const size_t TaskNameLen = 32;

class Task final : public Object
{
public:

    /* 64 KB. Eight pages were enough until the kernel grew a TLS client:
       verifying a certificate chain walks candidate issuers recursively and
       goes several times deeper than anything else a task does, and a task
       stack that overflows takes the machine down with it. */
    static const ulong StackSize = 16 * Const::PageSize;
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

    /* A task named name, as it is. Not a format: a task is made through
       TAlloc, and a format handed on through a template reaches the
       constructor as a variable no compiler can check against its
       arguments -- "idle%u" was given a ulong that way, which nothing
       saw. A name to be formatted is made with SnPrintf first, where the
       check is. */
    explicit Task(const char* name);

    bool Run(class TaskQueue& taskQueue, Func func, void* ctx);

    static Task* GetCurrentTask();
    static Task* TryGetCurrentTask();
    static void DiagnoseGetCurrentTask();

    virtual void Get() override;
    virtual void Put() override;

    bool Start(Func func, void* ctx);

    /* Waits for the task to exit, and says so when that takes long: once
       reportNs have passed -- who waits, for which task, for how long, and
       where that task is (Locate) -- and again each time the wait has
       doubled, or JoinReportMaxNs on, whichever is sooner. It never gives
       up: a task that has not exited is still using whatever its waiter is
       about to free. A join that never ends is a stop, an unload or a
       shutdown that never ends, and one that says nothing is the worst
       outcome there is: the report is the TLB shootdown's naming of the
       CPUs that never answered, for the tasks every stop path waits on. */
    void Wait(ulong reportNs = JoinReportNs);

    static const ulong JoinReportNs = 10 * Const::NanoSecsInSec;
    static const ulong JoinReportMaxNs = 300 * Const::NanoSecsInSec;

    /* Waits that Wait has reported, since boot */
    static long GetJoinReportCount();

    static const size_t WhereFrames = 16;

    /* Where a task is, as Locate found it */
    struct Where
    {
        enum Kind : ulong
        {
            /* Not started, or never still long enough to be read */
            Unknown,
            Exited,
            /* Off every CPU: blocked, asleep, or runnable and waiting for
               one. Frames as its switch left them. */
            Switched,
            /* On Cpu, where the interrupt that asked it landed: at Pc,
               Frames from there. The asker's own task too, Pc 0. */
            Running,
            /* Not off its CPU, and SilentCpus took no interrupt in
               AnswerNs to say whether it is on theirs: interrupts off
               there, which is where such a task usually is. */
            Silent,
        };

        Kind What;
        bool Blocked;
        bool Asleep;
        /* Woken (RunWaking), and not yet put back on its queue's run list:
           a moment's state, and a task found in it for long is a wakeup
           its CPU has not taken */
        bool Waking;
        ulong Cpu;
        ulong Pc;
        ulong SilentCpus;
        ulong AnswerNs;
        size_t Count;
        ulong Frames[WhereFrames];

        /* What, in words, into buf: "asleep", "running on cpu 3 at
           Spin+0x1c", ... */
        void Describe(char* buf, size_t size);
    };

    /* Where this task is, now: its return addresses, innermost first, and
       for a task on a CPU which one and where it was. A task off its CPU is
       read off its stack, and held to not having run while it was read; one
       on a CPU is asked, by an IPI to every other CPU, whose answers are
       waited for up to LocateAnswerNs -- a CPU with interrupts off takes no
       IPI, and is named instead. From task context, since it waits; one at
       a time across the machine. */
    void Locate(Where& where, ulong answerNs = LocateAnswerNs);

    static const ulong LocateAnswerNs = 1 * Const::NanoSecsInSec;

    void SetStopping();
    bool IsStopping();

    PRINTF_FORMAT(2, 3) void SetName(const char *fmt, ...);
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
    /* Back to an ordinary task: scheduled in its turn, not only when
       nothing else on its queue can run (see TaskQueue::ReleaseIdle) */
    void ClearIdle();
    bool IsIdle();

    /* The idle task is the last resort only while it idles -- inside
       Cpu::Idle, about to halt or halted. Running anything else -- the
       boot, its self-tests, the shutdown -- it is a task like any other and
       takes its turn: it used to be the last resort whatever it ran, and a
       task the boot started on the boot CPU that never blocked kept the
       boot itself off that CPU for as long as it ran. Set and cleared by
       the idle task itself, read by its own CPU's scheduler. */
    void SetIdling(bool idling);
    bool IsIdling();

    /* A task that has asked not to be scheduled until someone wakes it.
       Block() is called by the task itself, immediately before the Schedule()
       that switches it out; Unblock() may be called from any CPU and from
       hard IRQ context. Both are locked operations on one word, RunState,
       and take no lock: an Unblock() that finds the task already off its
       CPU puts it back on its queue's run list by way of that queue's wake
       stack (TaskQueue::PushWake), which takes none either. */
    void Block();
    void Unblock();
    bool IsBlocked();

    /* Where a task is with its queue -- RunState's values. Queued: it can
       run, and is on its queue's run list or on the CPU. Blocking: it has
       called Block() and is still on its CPU -- its Schedule() has not taken
       it off. Sleeping: that Schedule() has, and it is on no list but the
       sleep list, if it is asleep until a deadline. Waking: an Unblock()
       found it Sleeping and is handing it back (TaskQueue::PushWake). Each
       move is a compare-and-swap, so of an Unblock() and the Schedule() that
       takes the task off, or of a deadline and an Unblock(), exactly one
       wins: Blocking -> Queued is the waker's, Blocking -> Sleeping its
       CPU's, Sleeping -> Waking a waker's, Sleeping -> Queued the deadline's,
       Waking -> Queued its CPU's, as it puts it on the run list. */
    static const long RunQueued = 0;
    static const long RunBlocking = 1;
    static const long RunSleeping = 2;
    static const long RunWaking = 3;

    TaskQueue* SelectNextTaskQueue();

    static const long StateWaiting = 1;
    static const long StateRunning = 2;
    static const long StateExited = 3;

    static const long FlagStoppingBit = 1;

    /* The CPU's idle task. Marked so the scheduler can treat it as the last
       resort it is, rather than as one more task to take a turn. */
    static const long FlagIdleBit = 2;

    /* The idle task inside Cpu::Idle (SetIdling) */
    static const long FlagIdlingBit = 4;

    /* Shown in ps's flags while the task is blocked, as it always was; what
       says so is RunState */
    static const long FlagBlockedBit = 3;

public:
    /* On its queue's list of every task it is home to, or the exited list */
    Stdlib::ListEntry ListEntry;
    Stdlib::ListEntry TableListEntry;

    /* What its queue's EEVDF knows of it -- its weight, slice, virtual
       runtime, deadline and lag -- and its place on the queue's run list
       (Sched.Entry) while it can run and is off the CPU; and its place on
       the sleep list while asleep until a deadline. All under the queue's
       lock; set from elsewhere through SetTaskNice and SetTaskSlice. */
    SchedEntity Sched;
    Stdlib::ListEntry SleepEntry;

    /* RunQueued, RunBlocking, RunSleeping or RunWaking (above) */
    Atomic RunState;

    /* The next on the wake stack it is on (TaskQueue::PushWake): the
       waker's to write before the push publishes it, its queue's CPU's to
       read once it has taken the stack */
    Task* WakeNext;

    /* When it went on the run list, and whether a wait that long has been
       reported (TaskQueue::CheckStalls): under the queue's lock */
    ulong QueuedAt;
    bool StallReported;

    TaskQueue* TaskQueue;
    SpinLock Lock;
    Atomic PreemptDisableCounter;
    /* A reschedule that came while PreemptDisableCounter was up: the tick's,
       or the one an IPI asks for on behalf of a task just woken. The task
       keeps the CPU, as it must, and the reschedule is made the moment the
       count is back to zero (PreemptEnable), where it used to be dropped. */
    Atomic PreemptPending;
    Atomic ContextSwitches;
    ulong Rsp;

    Atomic State;
    Atomic Flags;
    /* The boot time, in nanoseconds, at which a task blocked in Sleep() is
       to run again: the scheduler's walk on its CPU notices that the time
       has come (TaskQueue::SelectNext) -- the tick's walk at the latest --
       and runs it, and the task unblocks itself. 0 while it is not asleep. */
    Atomic SleepUntil;

    /* When an Unblock() last took the flag down: the start of the wait for
       the CPU that SchedStats measures at the switch in. Written by the
       waker, from any CPU and any context. */
    Atomic WakeTime;

    /* It left the CPU able to run -- displaced, not blocked: the wait until
       it runs again is a delay, not a wakeup's (SchedStats). Under Lock. */
    bool SwitchedOutRunnable;

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

    /* Wait's report, after waitedNs */
    void ReportJoin(ulong waitedNs);

    /* Locate, under its lock; and its case of a task off its CPU, false if
       the task ran while being read */
    void LocateLocked(Where& where, ulong answerNs);
    bool LocateSwitched(Where& where);

    /* Looks a task Locate finds in neither place keeps getting: off its
       CPU when asked on one, on one when read off its stack */
    static const ulong LocateAttempts = 4;

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