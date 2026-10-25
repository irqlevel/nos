#include "task.h"
#include "stack_probe.h"
#include "trace.h"
#include <hal/cpu.h>
#include <hal/context.h>
#include <hal/barrier.h>
#include "sched.h"
#include "cpu.h"
#include "sched.h"
#include "preempt.h"
#include "mutex.h"
#include "stack_trace.h"
#include "symtab.h"
#include <mm/new.h>

namespace Kernel
{

Task::Task()
    : TaskQueue(nullptr)
    , Rsp(0)
    , State(0)
    , Flags(0)
    , SleepUntil(0)
    , WakeTime(0)
    , SwitchedOutRunnable(false)
    , Prev(nullptr)
    , Magic(TaskMagic)
    , CpuAffinity(~(0UL))
    , Pid(InvalidObjectId)
    , StackPtr(nullptr)
    , Function(nullptr)
    , Ctx(nullptr)
{
    RefCounter.Set(1);
    ListEntry.Init();
    Name[0] = '\0';
}

Task::Task(const char* name)
    : Task()
{
    /* Cut as a format's output is, marked where it is cut */
    Stdlib::SnPrintf(Name, Stdlib::ArraySize(Name), "%s", name);
    Trace(TaskLL, "task 0x%p %s", this, Name);
}

Task::~Task()
{
    BugOn(TaskQueue != nullptr);
    BugOn(StackPtr != nullptr);
    Trace(TaskLL, "task 0x%p %s dtor", this, Name);
}

void Task::Release()
{
    BugOn(TaskQueue != nullptr);

    if (StackPtr != nullptr)
    {
        Trace(TaskLL, "task 0x%p %s free stack 0x%lX",
            this, Name, (ulong)StackPtr);
        delete StackPtr;
        StackPtr = nullptr;
    }
}

void Task::Get()
{
    BugOn(RefCounter.Get() <= 0);
    RefCounter.Inc();
}

void Task::Put()
{
    BugOn(RefCounter.Get() <= 0);
    if (RefCounter.DecAndTest())
    {
        Release();
        delete this;
    }
}

void Task::SetStopping()
{
    Flags.SetBit(FlagStoppingBit);
}

bool Task::IsStopping()
{
    return Flags.TestBit(FlagStoppingBit);
}

void Task::SetIdle()
{
    Flags.SetBit(FlagIdleBit);
}

void Task::ClearIdle()
{
    Flags.ClearBit(FlagIdleBit);
}

bool Task::IsIdle()
{
    return Flags.TestBit(FlagIdleBit);
}

void Task::Block()
{
    Flags.SetBit(FlagBlockedBit);
}

void Task::Unblock()
{
    /* The clear stays a locked operation whether or not the flag is up: it
       orders the waker's publishing of the work before it, which a plain
       read of the flag would not on arm64. The clock only for a wakeup that
       took the flag down. */
    if (Flags.ClearBit(FlagBlockedBit))
        WakeTime.Set((long)GetBootTime().GetValue());
}

bool Task::IsBlocked()
{
    return Flags.TestBit(FlagBlockedBit);
}

void Task::Exit()
{
    BugOn(this != GetCurrentTask());

    ExitTime = GetBootTime();
    TaskTable::GetInstance().Remove(this);
    State.Set(StateExited);

    Schedule();

    Panic("Can't be here");
}

void Task::ExecCallback()
{
    Task* curr = GetCurrentTask();
    if (this != curr)
    {
        DiagnoseGetCurrentTask();
        Trace(0, "ExecCallback: this 0x%lX GetCurrentTask 0x%lX rsp 0x%lX",
            (ulong)this, (ulong)curr, Hal::GetSp());
        BugOn(true);
    }
    StartTime = GetBootTime();
    Function(Ctx);
    Exit();
}

void Task::Exec(void *task)
{
    static_cast<Task *>(task)->ExecCallback();
}

/* Waits that Wait has reported (GetJoinReportCount) */
static Atomic JoinReports;

void Task::Wait(ulong reportNs)
{
    const ulong start = GetBootTime().GetValue();
    ulong reportAt = reportNs;

    while (State.Get() != StateExited)
    {
        Sleep(1 * Const::NanoSecsInMs);

        ulong waited = GetBootTime().GetValue() - start;
        if (waited < reportAt)
            continue;

        ReportJoin(waited);
        reportAt = waited + Stdlib::Min(waited, JoinReportMaxNs);
    }
}

long Task::GetJoinReportCount()
{
    return JoinReports.Get();
}

/* Locate, one at a time across the machine: what it leaves with each CPU it
   asks (TaskProbe) is the next asker's to reuse, and a report's lines stay
   together */
static Mutex& LocateLock()
{
    static Mutex lock;
    return lock;
}

static void TaskProbeIpi(void* ctx, Context* ipiCtx);

/* What Locate leaves with a CPU it asks whether it is running a task. One
   per CPU, made the first time that CPU is asked and kept for good: a CPU
   with interrupts off never takes the IPI, and Locate stops waiting for it,
   so the IPI queued there -- and the answer its handler writes -- has to
   outlive the call that queued it. On the asker's stack, a CPU that took
   it late would write into whatever that stack had become by then. Under
   LocateLock, but for the answer, which the handler writes before it brings
   the IPI's Completion back to 0. */
struct TaskProbe final
{
    TaskProbe()
        : Ipi(TaskProbeIpi, this)
        , Want(nullptr)
        , Found(false)
        , Pc(0)
        , Count(0)
        , Frames{}
        , Pending(false)
    {
        /* Nothing asked yet, which is the same as answered */
        Ipi.Completion.Done();
    }

    IPITask Ipi;
    Task* Want;
    bool Found;
    ulong Pc;
    size_t Count;
    ulong Frames[Task::WhereFrames];
    /* Sent, and not answered when Locate stopped waiting */
    bool Pending;

    static const ulong Tag = 'TPrb';
};

static TaskProbe* TaskProbes[MaxCpus];

/* The IPI, on the CPU asked: whether the task it landed on -- the one whose
   stack the handler runs on, since neither arch switches stacks for an
   interrupt taken in the kernel -- is the task wanted, and if it is, where
   it was */
static void TaskProbeIpi(void* ctx, Context* ipiCtx)
{
    auto* probe = static_cast<TaskProbe*>(ctx);
    probe->Found = (Task::TryGetCurrentTask() == probe->Want);
    if (!probe->Found)
        return;

    probe->Pc = ipiCtx->GetRetRip();
    probe->Count = StackTrace::CaptureFromSp(ipiCtx->GetFramePointer(),
        ipiCtx->GetOrigRsp(), probe->Frames, Stdlib::ArraySize(probe->Frames));
}

/* Asks every other CPU running whether it is running task, and waits up to
   answerNs for the answers. True when one was -- where says which,
   and where it was -- and when none was but some CPU did not answer: the
   task is on one of those, as far as anything can tell. False when every
   CPU answered no: it has left its CPU since it was seen on one. */
static bool LocateRunning(Task* task, Task::Where& where, ulong answerNs)
{
    auto& cpus = CpuTable::GetInstance();
    ulong ask = cpus.GetRunningCpusNoLock() & ~(1UL << cpus.GetCurrentCpuId());
    ulong asked = 0;
    ulong silent = 0;

    for (ulong i = 0; i < MaxCpus; i++)
    {
        if (!(ask & (1UL << i)))
            continue;

        if (TaskProbes[i] == nullptr)
            TaskProbes[i] = Mm::TAlloc<TaskProbe, TaskProbe::Tag>();

        TaskProbe* probe = TaskProbes[i];
        if (probe == nullptr)
            continue;

        if (probe->Pending)
        {
            if (probe->Ipi.Completion.GetCounter() != 0)
            {
                /* Still has not taken the one asked before */
                silent |= 1UL << i;
                continue;
            }
            probe->Pending = false;
        }

        probe->Want = task;
        probe->Found = false;
        probe->Pc = 0;
        probe->Count = 0;
        probe->Ipi.Completion.Add(1);
        cpus.GetCpu(i).QueueIPITaskAsync(probe->Ipi);
        asked |= 1UL << i;
    }

    const ulong deadline = GetBootTime().GetValue() + answerNs;
    bool found = false;

    for (ulong i = 0; i < MaxCpus; i++)
    {
        if (!(asked & (1UL << i)))
            continue;

        TaskProbe* probe = TaskProbes[i];
        while (probe->Ipi.Completion.GetCounter() != 0 &&
               GetBootTime().GetValue() < deadline)
        {
            Sleep(Const::NanoSecsInMs);
        }

        if (probe->Ipi.Completion.GetCounter() != 0)
        {
            probe->Pending = true;
            silent |= 1UL << i;
            continue;
        }

        if (probe->Found && !found)
        {
            found = true;
            where.What = Task::Where::Running;
            where.Cpu = i;
            where.Pc = probe->Pc;
            where.Count = probe->Count;
            Stdlib::MemCpy(where.Frames, probe->Frames, probe->Count * sizeof(ulong));
        }
    }

    if (found)
        return true;

    if (silent == 0)
        return false;

    where.What = Task::Where::Silent;
    where.SilentCpus = silent;
    return true;
}

bool Task::LocateSwitched(Where& where)
{
    if (StackPtr == nullptr)
        return false;

    /* A seqlock's read, with the task's own switches for the sequence: what
       is read off its stack between the two looks is what its last switch
       left there only if no switch in or out came between them. Switched
       back in and still running, its State says so; switched in and out
       again, its count does. The stack stays mapped through all of it --
       the caller holds a reference, and Release frees it on the last -- and
       a stale stack pointer or frame chain is kept inside it by the bounds,
       so a look that loses the race names wrong frames and is thrown away,
       and never faults. */
    long switches = ContextSwitches.Get();
    ulong base = (ulong)StackPtr;
    ulong limit = (ulong)&StackPtr->StackTop[0];
    ulong sp = __atomic_load_n(&Rsp, __ATOMIC_RELAXED);

    size_t count = StackTrace::CaptureFrom(Hal::TaskSavedFramePointer(sp, base, limit),
        base, limit, where.Frames, WhereFrames);

    Hal::SmpRmb();
    if (State.Get() != StateWaiting || ContextSwitches.Get() != switches)
        return false;

    where.What = Where::Switched;
    where.Blocked = IsBlocked();
    where.Asleep = where.Blocked && SleepUntil.Get() != 0;
    where.Count = count;
    return true;
}

void Task::LocateLocked(Where& where, ulong answerNs)
{
    where.What = Where::Unknown;
    where.Blocked = false;
    where.Asleep = false;
    where.Cpu = 0;
    where.Pc = 0;
    where.SilentCpus = 0;
    where.AnswerNs = answerNs;
    where.Count = 0;

    if (this == TryGetCurrentTask())
    {
        where.What = Where::Running;
        where.Cpu = CpuTable::GetInstance().GetCurrentCpuId();
        where.Count = StackTrace::Capture(where.Frames, WhereFrames);
        return;
    }

    for (ulong attempt = 0; attempt < LocateAttempts; attempt++)
    {
        long state = State.Get();
        if (state == StateExited)
        {
            where.What = Where::Exited;
            return;
        }

        if (state == StateWaiting)
        {
            if (LocateSwitched(where))
                return;
        }
        else if (state == StateRunning)
        {
            if (LocateRunning(this, where, answerNs))
                return;
        }
        else
        {
            /* Not started */
            return;
        }
    }
}

void Task::Locate(Where& where, ulong answerNs)
{
    Stdlib::AutoLock lock(LocateLock());
    LocateLocked(where, answerNs);
}

void Task::Where::Describe(char* buf, size_t size)
{
    char pc[SymbolTable::DescribeMax];

    switch (What)
    {
    case Exited:
        Stdlib::SnPrintf(buf, size, "exited");
        break;
    case Switched:
        Stdlib::SnPrintf(buf, size, "%s",
            Asleep ? "asleep" : (Blocked ? "blocked" : "runnable, waiting for a cpu"));
        break;
    case Running:
        if (Pc != 0 && SymbolTable::GetInstance().Describe(Pc, pc, sizeof(pc)))
            Stdlib::SnPrintf(buf, size, "running on cpu %lu at %s", Cpu, pc);
        else if (Pc != 0)
            Stdlib::SnPrintf(buf, size, "running on cpu %lu at 0x%lX", Cpu, Pc);
        else
            Stdlib::SnPrintf(buf, size, "running on cpu %lu", Cpu);
        break;
    case Silent:
        Stdlib::SnPrintf(buf, size, "on a cpu that would not answer: cpus 0x%lX took no interrupt in %lu ms",
            SilentCpus, AnswerNs / Const::NanoSecsInMs);
        break;
    case Unknown:
    default:
        Stdlib::SnPrintf(buf, size, "not started, or never still long enough to be read");
        break;
    }
}

void Task::ReportJoin(ulong waitedNs)
{
    JoinReports.Inc();

    Stdlib::AutoLock lock(LocateLock());

    Where where;
    LocateLocked(where, LocateAnswerNs);

    char what[2 * SymbolTable::DescribeMax];
    where.Describe(what, sizeof(what));

    Task* waiter = TryGetCurrentTask();
    Trace(0, "join: %s (pid %lu) has waited %lu ms for %s (pid %lu) to exit, and it is %s",
        (waiter != nullptr) ? waiter->GetName() : "?",
        (waiter != nullptr) ? waiter->Pid : InvalidObjectId,
        waitedNs / Const::NanoSecsInMs, GetName(), Pid, what);

    auto& symtab = SymbolTable::GetInstance();
    char frame[SymbolTable::DescribeMax];
    for (size_t i = 0; i < where.Count; i++)
    {
        if (symtab.DescribeReturn(where.Frames[i], frame, sizeof(frame)))
            Trace(0, "join:   [%lu] 0x%lX %s", (ulong)i, where.Frames[i], frame);
        else
            Trace(0, "join:   [%lu] 0x%lX", (ulong)i, where.Frames[i]);
    }
}

bool Task::PrepareStart(Func func, void* ctx)
{
    BugOn(StackPtr != nullptr);
    BugOn(Function != nullptr);

    StackPtr = Mm::TAlloc<Stack, Tag>(this);
    if (StackPtr == nullptr)
    {
        return false;
    }

    /* Before the task runs, so what survives the fill is exactly what it
       never touched. */
    StackProbe::Poison(&StackPtr->StackBottom[0], sizeof(StackPtr->StackBottom));

    Trace(TaskLL, "task 0x%p %s stack 0x%lX top 0x%lX",
        this, Name, (ulong)StackPtr, (ulong)&StackPtr->StackTop[0]);

    if (!TaskTable::GetInstance().Insert(this))
    {
        delete StackPtr;
        StackPtr = nullptr;
        return false;
    }

    Function = func;
    Ctx = ctx;

    return true;
}

bool Task::Start(Func func, void* ctx)
{
    if (!PrepareStart(func, ctx))
        return false;

    Rsp = Hal::BuildTaskFrame((ulong)&StackPtr->StackTop[0],
        (ulong)&Task::Exec, (ulong)this);

    StartTime = GetBootTime();
    State.Set(StateWaiting);

    auto taskQueue = SelectNextTaskQueue();
    if (taskQueue == nullptr)
        return false;
    taskQueue->Insert(this);
    return true;
}

bool Task::Run(class TaskQueue& taskQueue, Func func, void* ctx)
{
    if (!PrepareStart(func, ctx))
        return false;

    StartTime = GetBootTime();
    RunStartTime = GetBootTime();
    State.Set(StateRunning);

    taskQueue.Insert(this);

    /* Never returns: Run is only used for the idle/boot task bodies */
    Hal::RunOnStack((ulong)&StackPtr->StackTop[0], Function, Ctx);
}

Task* Task::GetCurrentTask()
{
    /* Derive the task from RSP by rounding down to StackSize boundary.
       Returns nullptr if the stack doesn't look valid (e.g. during SwitchComplete
       before the new stack is fully set up, or on a non-task stack).
       NOTE: no Trace() calls here — Trace takes a lock which calls
       PreemptDisable -> GetCurrentTask, causing infinite recursion. */
    ulong rsp = Hal::GetSp();
    struct Stack* stackPtr = reinterpret_cast<struct Stack *>(rsp & (~(StackSize - 1)));
    if (BugOn(stackPtr->Magic1 != StackMagic1))
        return nullptr;

    if (BugOn(stackPtr->Magic2 != StackMagic2))
        return nullptr;

    if (BugOn(rsp < ((ulong)&stackPtr->StackBottom[0] + Const::PageSize)))
        return nullptr;

    if (BugOn(rsp > (ulong)&stackPtr->StackTop[0]))
        return nullptr;

    Task* task = stackPtr->Task;
    if (BugOn(task->Magic != TaskMagic))
        return nullptr;

    return task;
}

Task* Task::TryGetCurrentTask()
{
    /*
     * Same as GetCurrentTask but without BugOn — returns nullptr
     * on any invalid state. Safe to call from panic context.
     */
    ulong rsp = Hal::GetSp();
    struct Stack* stackPtr = reinterpret_cast<struct Stack *>(rsp & (~(StackSize - 1)));
    if (stackPtr->Magic1 != StackMagic1 || stackPtr->Magic2 != StackMagic2)
        return nullptr;

    if (rsp < ((ulong)&stackPtr->StackBottom[0] + Const::PageSize))
        return nullptr;

    if (rsp > (ulong)&stackPtr->StackTop[0])
        return nullptr;

    Task* task = stackPtr->Task;
    if (task == nullptr || task->Magic != TaskMagic)
        return nullptr;

    return task;
}

void Task::DiagnoseGetCurrentTask()
{
    /* Safe to call from contexts where Trace is allowed (e.g. Schedule).
       Re-checks the same conditions as GetCurrentTask but logs the failure. */
    ulong rsp = Hal::GetSp();
    struct Stack* stackPtr = reinterpret_cast<struct Stack *>(rsp & (~(StackSize - 1)));

    Trace(0, "DiagTask: rsp 0x%lX base 0x%lX", rsp, (ulong)stackPtr);
    Trace(0, "DiagTask: Magic1 0x%lX expect 0x%lX", stackPtr->Magic1, StackMagic1);
    Trace(0, "DiagTask: Magic2 0x%lX expect 0x%lX", stackPtr->Magic2, StackMagic2);
    Trace(0, "DiagTask: StackBottom+page 0x%lX StackTop 0x%lX",
        (ulong)&stackPtr->StackBottom[0] + Const::PageSize,
        (ulong)&stackPtr->StackTop[0]);

    if (stackPtr->Magic1 == StackMagic1 && stackPtr->Magic2 == StackMagic2)
    {
        Task* task = stackPtr->Task;
        Trace(0, "DiagTask: Task 0x%lX TaskMagic 0x%lX expect 0x%lX",
            (ulong)task, task->Magic, TaskMagic);
    }
}

void Task::SetName(const char *fmt, ...)
{
    Name[0] = '\0';

    va_list args;
    va_start(args, fmt);
    Stdlib::VsnPrintf(Name, Stdlib::ArraySize(Name), fmt, args);
    va_end(args);
}

const char* Task::GetName()
{
    return Name;
}

void Task::UpdateRuntime()
{
    auto now = GetBootTime();
    Runtime += (now - RunStartTime);
    RunStartTime = now;
}

void Task::SetCpuAffinity(ulong affinity)
{
    Stdlib::AutoLock lock(Lock);
    CpuAffinity = affinity;
}

ulong Task::GetCpuAffinity()
{
    Stdlib::AutoLock lock(Lock);
    return CpuAffinity;
}

TaskQueue* Task::SelectNextTaskQueue()
{
    /* The mirror, not the locked walk: this runs on every context switch,
       and GetRunningCpus takes the table lock and then each of the 64 CPUs'
       own locks to read one flag apiece. Nothing ever clears StateRunning,
       so the two answers agree once a CPU has published itself. */
    ulong cpuMask = CpuTable::GetInstance().GetRunningCpusNoLock() & CpuAffinity;
    if (cpuMask == 0)
        return nullptr;

    /* Read once: Remove() clears it, and the caller may be about to. */
    class TaskQueue* current = TaskQueue;

    class TaskQueue* best = nullptr;
    long bestCount = 0;

    for (ulong i = 0; i < MaxCpus; i++)
    {
        if (!(cpuMask & (1UL << i)))
            continue;

        auto& cand = CpuTable::GetInstance().GetCpu(i).GetTaskQueue();
        long count = cand.GetTaskCount();

        /* The queue this task is already on competes like any other. It used
           to be skipped outright, which meant every task was moved to another
           CPU on every context switch -- not balancing, just motion, and it
           threw away whatever the task had warm in that core's caches. */
        if (best == nullptr || count < bestCount)
        {
            best = &cand;
            bestCount = count;
        }
    }

    if (best == nullptr)
        return nullptr;

    /* Placing a task for the first time: there is nowhere to stay. */
    if (current == nullptr)
        return best;

    if (best == current)
        return nullptr;

    /* This task is still counted on its own queue, so moving it takes one
       off that side and puts one on the other: a difference of one would
       only be reversed on the next switch, and the task would trade places
       forever. Move for two. */
    if (bestCount + MigrateThreshold > current->GetTaskCount())
        return nullptr;

    return best;
}

TaskTable::TaskTable()
{
}

TaskTable::~TaskTable()
{
    for (size_t i = 0; i < Stdlib::ArraySize(TaskList); i++)
    {
        Stdlib::ListEntry taskList;

        {
            Stdlib::AutoLock lock(Lock[i]);
            taskList.MoveTailList(&TaskList[i]);
        }

        while (!taskList.IsEmpty())
        {
            Task* task = CONTAINING_RECORD(taskList.RemoveHead(), Task, TableListEntry);
            task->TableListEntry.Init();
            task->Put();
        }
    }
}

bool TaskTable::Insert(Task *task)
{
    ObjectId pid = TaskObjectTable.Insert(task);
    if (pid == InvalidObjectId)
    {
        return false;
    }
    task->Pid = pid;

    task->Get();
    size_t i = Stdlib::HashPtr(task) % Stdlib::ArraySize(TaskList);
    Stdlib::AutoLock lock(Lock[i]);

    BugOn(!task->TableListEntry.IsEmpty());
    TaskList[i].InsertTail(&task->TableListEntry);

    return true;
}

void TaskTable::Remove(Task *task)
{
    {
        size_t i = Stdlib::HashPtr(task) % Stdlib::ArraySize(TaskList);

        TaskObjectTable.Remove(task->Pid);

        Stdlib::AutoLock lock(Lock[i]);

        BugOn(task->TableListEntry.IsEmpty());
        task->TableListEntry.RemoveInit();
    }

    task->Put();
}

Task* TaskTable::Lookup(ulong pid)
{
    return reinterpret_cast<Task*>(TaskObjectTable.Lookup(pid));
}

size_t TaskTable::SampleCpu(CpuSample* out, size_t max)
{
    size_t n = 0;

    for (size_t i = 0; i < Stdlib::ArraySize(TaskList) && n < max; i++)
    {
        Stdlib::AutoLock lock(Lock[i]);

        size_t walked = 0;
        for (auto currEntry = TaskList[i].Flink;
            currEntry != &TaskList[i] && n < max;
            currEntry = currEntry->Flink)
        {
            if (++walked > MaxTasksPerList)
            {
                Trace(0, "SampleCpu: task list %lu does not end after %lu entries",
                    (ulong)i, (ulong)MaxTasksPerList);
                break;
            }

            Task* task = CONTAINING_RECORD(currEntry, Task, TableListEntry);

            out[n].Pid = task->Pid;
            out[n].RuntimeNs = task->Runtime.GetValue();
            Stdlib::MemCpy(out[n].Name, task->GetName(), sizeof(out[n].Name));
            out[n].Name[sizeof(out[n].Name) - 1] = '\0';
            n++;
        }
    }

    return n;
}

ulong Task::GetStackFree()
{
    if (StackPtr == nullptr)
        return 0;

    return StackProbe::Untouched(&StackPtr->StackBottom[0],
        sizeof(StackPtr->StackBottom));
}

void TaskTable::Stacks(Stdlib::Printer& printer, ulong& worstFree)
{
    for (size_t i = 0; i < Stdlib::ArraySize(TaskList); i++)
    {
        Stdlib::AutoLock lock(Lock[i]);

        size_t walked = 0;
        for (auto currEntry = TaskList[i].Flink;
            currEntry != &TaskList[i];
            currEntry = currEntry->Flink)
        {
            if (++walked > MaxTasksPerList)
            {
                printer.Printf("task list %lu does not end after %lu entries\n",
                    (ulong)i, (ulong)MaxTasksPerList);
                break;
            }

            Task* task = CONTAINING_RECORD(currEntry, Task, TableListEntry);
            ulong free = task->GetStackFree();
            if (free == 0)
                continue;

            printer.Printf("task %lu %lu %lu %s\n", task->Pid,
                (ulong)Task::StackSize - free, (ulong)Task::StackSize, task->GetName());

            if (free < worstFree)
                worstFree = free;
        }
    }
}

void TaskTable::Ps(Stdlib::Printer& printer)
{
    printer.Printf("pid state flags runtime ctxswitches name\n");

    for (size_t i = 0; i < Stdlib::ArraySize(TaskList); i++)
    {
        Stdlib::AutoLock lock(Lock[i]);

        size_t walked = 0;
        for (auto currEntry = TaskList[i].Flink;
            currEntry != &TaskList[i];
            currEntry = currEntry->Flink)
        {
            if (++walked > MaxTasksPerList)
            {
                printer.Printf("task list %lu does not end after %lu entries\n",
                    (ulong)i, (ulong)MaxTasksPerList);
                Trace(0, "Ps: task list %lu does not end after %lu entries",
                    (ulong)i, (ulong)MaxTasksPerList);
                break;
            }

            Task* task = CONTAINING_RECORD(currEntry, Task, TableListEntry);
            printer.Printf("%lu %lu 0x%lX %lu.%lu %lu %s\n",
                task->Pid, task->State.Get(), task->Flags.Get(), task->Runtime.GetSecs(),
                task->Runtime.GetUsecs(), task->ContextSwitches.Get(), task->GetName());
        }
    }
}

}