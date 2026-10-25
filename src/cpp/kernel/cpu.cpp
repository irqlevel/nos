#include "cpu.h"
#include "interrupt.h"
#include "panic.h"
#include "trace.h"
#include "watchdog.h"
#include "softirq.h"
#include "profiler.h"
#include "timer.h"

#include <hal/irqchip.h>
#include <hal/mmu.h>
#include <hal/timer.h>

#include <kernel/time.h>
#include <mm/new.h>
#include <mm/page_table.h>

/* The network layer is Rust (src/rust/net): a receive pass offered from the
   tick, which it takes only with rxpoll=on. */
extern "C" void rust_net_poll_rx();

namespace Kernel
{

IPITask::IPITask()
    : Function(nullptr)
    , Ctx(nullptr)
    , Completion(1)
{
}

IPITask::IPITask(Func func, void* ctx)
    : Function(func)
    , Ctx(ctx)
    , Completion(1)
{
}

Cpu::Cpu()
    : Index(0)
    , State(0)
    , IdleTaskPtr(nullptr)
    , IPITasksClosed(false)
    , TimerOneShot(false)
    , TickDue(0)
    , TimerArmedAt(~0UL)
    , TimerWalk(false)
{
    IPITaskList.Init();
}

ulong Cpu::GetIndex()
{
    return Index;
}

void Cpu::Idle()
{
    if (BugOn(!(State & StateRunning)))
        return;

    if (BugOn(!Hal::IsInterruptEnabled()))
        return;

    /* Free tasks that exited on this CPU (deferred out of the IRQs-off switch
       path); safe here because interrupts are enabled. */
    GetTaskQueue().ReapExited();

    /* The last resort from here until it wakes: an interrupt that finds
       something else to run switches to it and leaves this task in its
       place, rather than in the turns (Task::SetIdling). Not before the
       reap, whose frees may wait for other CPUs. */
    Task* self = Task::TryGetCurrentTask();
    if (self != nullptr)
        self->SetIdling(true);
    Hlt();
    if (self != nullptr)
        self->SetIdling(false);
}

ulong Cpu::GetState()
{
    Stdlib::AutoLock lock(Lock);
    return State;
}

bool Cpu::IsParked()
{
    return (Parked.Get() != 0) ? true : false;
}

void Cpu::SetRunning()
{
    {
        Stdlib::AutoLock lock(Lock);
        if (BugOn(State & StateRunning))
            return;

        State |= StateRunning;
    }

    /* Published outside this CPU's own lock and without the table's: the
       watchdog's tick path reads this mirror on every CPU. */
    CpuTable::GetInstance().NoteCpuRunning(Index);
}

void Cpu::SetExiting()
{
    Stdlib::AutoLock lock(Lock);
    State |= StateExiting;
}

void Cpu::Init(ulong index)
{
    Stdlib::AutoLock lock(Lock);
    if (BugOn(State & StateInited))
        return;

    Index = index;
    State |= StateInited;

    Trace(0, "Cpu 0x%p %lu inited", this, Index);
}

void Cpu::Reset()
{
    Stdlib::AutoLock lock(Lock);

    BugOn(State == StateRunning);
    TaskQueue.Clear();

    if (IdleTaskPtr != nullptr)
    {
        IdleTaskPtr->Put();
        IdleTaskPtr = nullptr;
    }
}

Cpu::~Cpu()
{
    Reset();
}

CpuTable::CpuTable()
    : BspIndex(0)
{
}

CpuTable::~CpuTable()
{
    Reset();
}

bool CpuTable::InsertCpu(ulong index)
{
    Stdlib::AutoLock lock(Lock);
    if (index >= Stdlib::ArraySize(CpuArray))
        return false;

    auto& cpu = CpuArray[index];
    if (cpu.GetState() & Cpu::StateInited)
        return false;

    cpu.Init(index);
    return true;
}

Cpu& CpuTable::GetCpu(ulong index)
{
    BugOn(index >= Stdlib::ArraySize(CpuArray));
    Cpu& cpu = CpuArray[index];
    return cpu;
}

ulong CpuTable::GetBspIndex()
{
    Stdlib::AutoLock lock(Lock);
    return GetBspIndexLockHeld();
}

ulong CpuTable::GetBspIndexLockHeld()
{
    return BspIndex;
}

ulong CpuTable::GetBspIndexNoLock()
{
    return (ulong)BspIndexCached.Get();
}

bool CpuTable::SetBspIndex(ulong index)
{
    Stdlib::AutoLock lock(Lock);

    if (BugOn(index >= Stdlib::ArraySize(CpuArray)))
        return false;

    auto& cpu = CpuArray[index];
    if (BugOn(!(cpu.GetState() & Cpu::StateInited)))
        return false;

    cpu.SetRunning();
    BspIndex = index;
    BspIndexCached.Set((long)index);
    return true;
}

ulong CpuTable::GetCurrentCpuId()
{
    return Hal::GetCurrentCpuHwId();
}

Cpu& CpuTable::GetCurrentCpu()
{
    return GetCpu(GetCurrentCpuId());
}

void CpuTable::ExitAllExceptSelf()
{
    auto& self = GetCurrentCpu();

    ulong cpuMask = GetRunningCpus();
    for (ulong i = 0; i < MaxCpus; i++)
    {
        if ((cpuMask & (1UL << i)) && (i != self.GetIndex()))
        {
            auto& cpu = GetCpu(i);
            cpu.SetExiting();

            while (!cpu.IsParked())
            {
                cpu.SendIPISelf();
                Pause();
            }
        }
    }
}

void CpuTable::SendIPIAllExclude(ulong excludeIndex)
{
    ulong cpuMask = GetRunningCpus();
    for (ulong i = 0; i < MaxCpus; i++)
    {
        if ((cpuMask & (1UL << i)) && (i != excludeIndex))
        {
            auto& cpu = GetCpu(i);

            cpu.SendIPISelf();
        }
    }
}

void CpuTable::SendIPIAll()
{
    ulong cpuMask = GetRunningCpus();
    for (ulong i = 0; i < MaxCpus; i++)
    {
        if ((cpuMask & (1UL << i)))
        {
            auto& cpu = GetCpu(i);

            cpu.SendIPISelf();
        }
    }
}

void Cpu::OnPanic()
{
    InterruptDisable();
    for (;;)
    {
        Pause();
    }
}

/* What a parked CPU stands on (Cpu::Park): a stack of its own, static, which
   nothing frees -- so that whatever a parked CPU still runs, it touches
   nothing the halt takes down. That is its last two calls and the halt; on
   x86 also an NMI or a machine check, which end HLT with interrupts off,
   push their frames onto the stack the CPU is on, and may panic there. A
   panic is what the size is for: one made to run on this stack (StackProbe,
   on a build for the purpose) went 1800 bytes deep on arm64 and 1880 on
   x86, and 5176 on arm64 with the netconsole sending its report. */
static const ulong ParkStackSize = 4 * Const::PageSize;
static char ParkStack[MaxCpus][ParkStackSize] __attribute__((aligned(16)));

/* Where an exiting CPU ends, for good. Once it is parked (IsParked) the CPU
   that asked goes on to free the tasks, their stacks and at last the heap --
   the stack this one was interrupted on among them -- so from that moment
   this CPU must touch none of it, for as long as the machine runs.

   It used to say so on that stack and stay there. One halt is not enough to
   stay put: arm64's WFI may complete at any moment -- a pending interrupt
   ends it even while masked, and under HVF every AP was out of it within
   milliseconds -- and x86's HLT ends at an NMI, whose frame goes onto the
   stack the CPU is on. The halt was a loop around Hlt(), a call, whose frame
   record went onto that stack at every wakeup: the data abort a parked AP
   printed now and then on arm64, in the middle of the boot CPU's static
   destructors. And the word that it had exited was written under this CPU's
   lock, whose release went on, past the moment the other CPU could see the
   word, to write the preemption count of the task it had interrupted.

   So it leaves that stack first, for its own, says it is parked from there
   -- an atomic store and not this CPU's lock, which off a task's stack would
   look for the current task and panic -- and halts in a loop that touches no
   memory at all. */
void Cpu::Park()
{
    InterruptDisable();

    Hal::RunOnStack((ulong)&ParkStack[Index][ParkStackSize], ParkOnOwnStack, this);
}

void Cpu::ParkOnOwnStack(void* ctx)
{
    auto cpu = static_cast<Cpu*>(ctx);

    cpu->Parked.Set(1);

    Hal::HaltForever();
}

static void TlbFlushFunc(void* ctx, Context* ipiCtx)
{
    (void)ctx;
    (void)ipiCtx;
    Mm::PageTable::InvalidateLocalTlb();
}

struct TlbRangeCtx
{
    ulong VirtAddr;
    ulong Count;
};

static void TlbFlushRangeFunc(void* ctx, Context* ipiCtx)
{
    (void)ipiCtx;
    auto* rc = (TlbRangeCtx*)ctx;
    Mm::PageTable::InvalidateLocalTlbRange(rc->VirtAddr, rc->Count);
}

/* Build a running-CPU mask excluding the local CPU and exited CPUs. */
ulong CpuTable::GetRemoteCpuMask()
{
    ulong localId = GetCurrentCpuId();
    ulong cpuMask = GetRunningCpus() & ~(1UL << localId);

    for (ulong i = 0; i < MaxCpus; i++)
    {
        if ((cpuMask & (1UL << i)) && CpuArray[i].IsParked())
            cpuMask &= ~(1UL << i);
    }

    return cpuMask;
}

void CpuTable::SendTlbIPI(ulong cpuMask, IPITask tasks[MaxCpus])
{
    /* A shootdown that never completes means some CPU cannot take the IPI,
       and in this kernel that has exactly one cause: it is spinning on a
       lock with interrupts off -- usually a lock this CPU is holding while
       it frees the page it is shooting down. Waiting for it forever produces
       a machine that stops dead with no fault to report and, if the lock is
       in the network path, no way to report it either. Ten seconds is far
       past any legitimate round of IPIs; name the CPUs that never answered
       and panic, which at least prints and can be read afterwards. */
    static const ulong AckTimeoutNs = 10 * Const::NanoSecsInSec;

    for (ulong i = 0; i < MaxCpus; i++)
    {
        if (cpuMask & (1UL << i))
            CpuArray[i].QueueIPITaskAsync(tasks[i]);
    }

    Stdlib::Time deadline = GetBootTime() + Stdlib::Time(AckTimeoutNs);
    ulong silent = 0;

    for (ulong i = 0; i < MaxCpus; i++)
    {
        if (!(cpuMask & (1UL << i)))
        {
            tasks[i].Completion.Done();
            continue;
        }

        while (tasks[i].Completion.GetCounter() != 0)
        {
            if (GetBootTime() >= deadline)
            {
                silent |= (1UL << i);
                break;
            }

            /* YieldToRunnable, not Schedule: the ack comes from the remote
               CPU's IPI handler, which has no reason to interrupt this one.
               Handing this CPU to the idle task therefore halts it until its
               own next tick, and a shootdown that the other CPUs answered in
               microseconds took a whole one -- on the path every Mm::Free
               with a mapping to drop goes down (see WaitGroup::Wait). */
            YieldToRunnable();
        }
    }

    if (silent != 0)
        Panic("TLB shootdown: cpus 0x%lX never acknowledged (asked 0x%lX)",
            silent, cpuMask);
}

void CpuTable::InvalidateTlbAll()
{
    Mm::PageTable::InvalidateLocalTlb();

    if (!Hal::TlbShootdownNeedsIpi())
        return; /* the local TLBI already broadcast to all CPUs */

    ulong cpuMask = GetRemoteCpuMask();
    if (cpuMask == 0)
        return;

    IPITask tasks[MaxCpus];

    for (ulong i = 0; i < MaxCpus; i++)
    {
        if (cpuMask & (1UL << i))
            tasks[i].Function = TlbFlushFunc;
    }

    SendTlbIPI(cpuMask, tasks);
}

void CpuTable::InvalidateTlbAddress(ulong virtAddr)
{
    Mm::PageTable::InvalidateLocalTlbAddress(virtAddr);

    if (!Hal::TlbShootdownNeedsIpi())
        return;

    ulong cpuMask = GetRemoteCpuMask();
    if (cpuMask == 0)
        return;

    TlbRangeCtx rc;
    rc.VirtAddr = virtAddr;
    rc.Count = 1;

    IPITask tasks[MaxCpus];

    for (ulong i = 0; i < MaxCpus; i++)
    {
        if (cpuMask & (1UL << i))
        {
            tasks[i].Function = TlbFlushRangeFunc;
            tasks[i].Ctx = &rc;
        }
    }

    SendTlbIPI(cpuMask, tasks);
}

void CpuTable::InvalidateTlbRange(ulong virtAddr, ulong count)
{
    Mm::PageTable::InvalidateLocalTlbRange(virtAddr, count);

    if (!Hal::TlbShootdownNeedsIpi())
        return;

    ulong cpuMask = GetRemoteCpuMask();
    if (cpuMask == 0)
        return;

    TlbRangeCtx rc;
    rc.VirtAddr = virtAddr;
    rc.Count = count;

    IPITask tasks[MaxCpus];

    for (ulong i = 0; i < MaxCpus; i++)
    {
        if (cpuMask & (1UL << i))
        {
            tasks[i].Function = TlbFlushRangeFunc;
            tasks[i].Ctx = &rc;
        }
    }

    SendTlbIPI(cpuMask, tasks);
}

void Cpu::IPI(Context* ctx)
{
    IPIConter.Inc();

    ProcessIPITasks(ctx);

    if (Panicker::GetInstance().IsActive())
    {
        OnPanic();
        return;
    }

    bool exit;
    {
        Stdlib::AutoLock lock(Lock);
        exit = (State & StateExiting) ? true : false;
    }

    if (exit)
    {
        Trace(0, "Cpu %lu exited, state 0x%lX, IPI count %lu",
            Index, State, IPIConter.Get());

        /* Complete any IPI tasks queued during the exit transition and refuse
           future ones, so a CPU that queued work to us never waits forever. */
        DrainAndCloseIPITasks(ctx);

        Park();
    }

    /* The periodic work moved to this CPU's own timer (Cpu::TimerTick).
       While that timer is running the IPI carries only queued work; on a
       machine where it could not be calibrated, the tick still arrives here
       as a broadcast from whichever CPU owns the HPET. */
    if (!CpuTable::GetInstance().HasPerCpuTimer())
    {
        Watchdog::GetInstance().Check();

        /* Index is the LAPIC APIC ID; the BSP's is not necessarily 0 */
        if (Index == CpuTable::GetInstance().GetBspIndexNoLock())
            TimerTable::GetInstance().ProcessTimers();
    }

    Hal::IrqEoi(CpuTable::IPIVector);

    /* Not Schedule(): a task this lands on while it can still run keeps the
       CPU -- see Preempt */
    Preempt();
}

/* The local timer, one-shot. It used to tick periodically and nothing else,
   and a task asleep until a deadline -- Sleep(), Event::WaitFor() -- ran at
   the first tick after it: a halted guest's vCPU waits for its guest's next
   timer edge that way, and a guest's `usleep 1000` took 10 ms on the EX44.
   Now the timer fires at the earlier of the next tick and the soonest such
   deadline on this CPU, and the scheduler's walk that follows runs the
   sleeper. The tick keeps its grid and its work; a deadline's interrupt only
   reschedules. */
void Cpu::LocalTimerStart()
{
    ulong flags = Hal::IrqSave();
    ulong now = GetBootTime().GetValue();

    TimerOneShot = true;
    TickDue = now + TickPeriodNs;
    LocalTimerArm(TickDue, now);

    Hal::IrqRestore(flags);
}

void Cpu::LocalTimerArm(ulong when, ulong now)
{
    ulong delta = (when > now) ? when - now : 0;

    /* Never further than a tick and its slack, whatever `when` says: a
       clock that stepped back would otherwise leave the CPU without a tick. */
    delta = Stdlib::Max(delta, TimerMinNs);
    delta = Stdlib::Min(delta, TickPeriodNs + TickSlackNs);

    TimerArmedAt = now + delta;
    Hal::LocalTimerArm(delta);
}

bool Cpu::LocalTimerFired(ulong now)
{
    if (!TimerOneShot)
        return true;

    bool tick = (now + TickSlackNs >= TickDue);
    if (tick)
    {
        /* On the grid; a tick missed -- interrupts off a long while -- is
           not made up, the next one a period from now */
        TickDue += TickPeriodNs;
        if (TickDue <= now)
            TickDue = now + TickPeriodNs;
    }

    /* The tick, for now: a sleeper's deadline sooner than it is brought
       forward by the walk the reschedule after this makes, under the queue
       lock it takes anyway (TaskQueue::Schedule) -- not from here, with
       interrupts off and whatever this CPU was doing interrupted. */
    LocalTimerArm(TickDue, now);
    TimerWalk = true;

    return tick;
}

bool Cpu::LocalTimerTakeWalk()
{
    bool walk = TimerWalk;
    TimerWalk = false;
    return walk;
}

void Cpu::LocalTimerWake(ulong deadline)
{
    if (!TimerOneShot || deadline == 0 || deadline >= TimerArmedAt)
        return;

    LocalTimerArm(deadline, GetBootTime().GetValue());
}

void Cpu::LocalTimerTick()
{
    if (!TimerOneShot)
        return;

    ulong now = GetBootTime().GetValue();
    if (TickDue <= now)
        TickDue = now + TickPeriodNs;
    LocalTimerArm(TickDue, now);
}

/* This CPU's own tick. One interrupt per CPU delivered by its own local
   APIC, instead of one interrupt on one CPU followed by an IPI to every
   other -- which cost a fan of ICR writes per tick and, worse, made the
   machine's entire sense of time depend on one CPU still answering
   interrupts. A CPU that wedges now stops only its own tick, and the others
   go on ticking and notice. One-shot, and also for a sleeper's deadline
   (LocalTimerStart): such an interrupt only reschedules. */
void Cpu::TimerTick(Context* ctx)
{
    if (Panicker::GetInstance().IsActive())
    {
        /* Ticking on, as the periodic timer did, without the walk: the
           queue's lock may be the panicking CPU's */
        LocalTimerTick();
        Hal::IrqEoi(CpuTable::TimerVector);
        return;
    }

    if (!LocalTimerFired(GetBootTime().GetValue()))
    {
        InterruptStats::Inc(IrqTimerWake);
        Hal::IrqEoi(CpuTable::TimerVector);
        Preempt();
        return;
    }

    InterruptStats::Inc(IrqLapicTimer);

    Watchdog::GetInstance().Check();

    Profiler::GetInstance().Tick(ctx);

    /* Software timers stay on one CPU: they are a handful of periodic
       callbacks, not a per-CPU wheel, and running them everywhere would
       just fire each one N times. */
    if (Index == CpuTable::GetInstance().GetBspIndexNoLock())
    {
        TimerTable::GetInstance().ProcessTimers();

        /* And offer the receive path a look, whether or not a NIC asked.
           The network layer takes it only with rxpoll=on -- the switch is
           in its poll, DeviceTable::poll_rx, not here. */
        rust_net_poll_rx();
    }

    /* Repairs a wakeup that went missing, and nothing else -- see
       SoftIrq::TickKick. Before the reschedule below, so the kick has an
       effect on this very tick. */
    SoftIrq::GetInstance().TickKick(Index);

    Hal::IrqEoi(CpuTable::TimerVector);

    /* Round-robin among the runnable, and never the idle task over a task
       that can still run -- see Preempt */
    Preempt();
}

void Cpu::QueueIPITaskAsync(IPITask& task)
{
    ulong flags = IPITaskLock.LockIrqSave();
    bool closed = IPITasksClosed;
    if (!closed)
        IPITaskList.InsertTail(&task.ListEntry);
    IPITaskLock.UnlockIrqRestore(flags);

    if (closed)
    {
        /* Target CPU has stopped processing IPI tasks (exiting/exited); the
           task will never run there, so complete it now to unblock the waiter
           instead of leaving it queued forever. */
        task.Completion.Done();
        return;
    }

    SendIPISelf();
}

void Cpu::QueueIPITask(IPITask& task)
{
    QueueIPITaskAsync(task);
    task.Completion.Wait();
}

void Cpu::ProcessIPITasks(Context* ctx)
{
    Stdlib::ListEntry localList;
    localList.Init();

    ulong flags = IPITaskLock.LockIrqSave();
    localList.MoveTailList(&IPITaskList);
    IPITaskLock.UnlockIrqRestore(flags);

    while (!localList.IsEmpty())
    {
        auto* entry = localList.RemoveHead();
        IPITask* task = CONTAINING_RECORD(entry, IPITask, ListEntry);
        task->Function(task->Ctx, ctx);
        task->Completion.Done();
    }
}

void Cpu::DrainAndCloseIPITasks(Context* ctx)
{
    Stdlib::ListEntry localList;
    localList.Init();

    /* Close the queue and take everything still on it under the same lock, so
       a concurrent QueueIPITaskAsync either lands here (and is completed below)
       or observes the closed flag and self-completes -- never both, never
       neither. */
    ulong flags = IPITaskLock.LockIrqSave();
    IPITasksClosed = true;
    localList.MoveTailList(&IPITaskList);
    IPITaskLock.UnlockIrqRestore(flags);

    while (!localList.IsEmpty())
    {
        auto* entry = localList.RemoveHead();
        IPITask* task = CONTAINING_RECORD(entry, IPITask, ListEntry);
        task->Function(task->Ctx, ctx);
        task->Completion.Done();
    }
}

TaskQueue& Cpu::GetTaskQueue()
{
    return TaskQueue;
}

extern "C" void IPInterrupt(Context* ctx)
{
    InterruptStats::Inc(IrqIPI);
    auto& cpu = CpuTable::GetInstance().GetCurrentCpu();
    cpu.IPI(ctx);
}

/* A posted interrupt's notification that found its vCPU out of its guest
   (Hal::PostedVector): the vCPU folds what was posted for it into its APIC
   before it next enters, so there is nothing to do but end the interrupt. */
extern "C" void PostedInterrupt(Context* ctx)
{
    (void)ctx;
    InterruptStats::Inc(IrqPosted);
    Hal::IrqEoi(Hal::PostedVector);
}

extern "C" void LapicTimerInterrupt(Context* ctx)
{
    auto& cpu = CpuTable::GetInstance().GetCurrentCpu();
    cpu.TimerTick(ctx);
}

void Cpu::SendIPISelf()
{
    Stdlib::AutoLock lock(Lock);

    if (BugOn(!(State & Cpu::StateRunning)))
        return;

    if (IsParked())
        return;

    Hal::SendIpi(Index, CpuTable::IPIVector);
}

void CpuTable::SendIPI(ulong index)
{
    Stdlib::AutoLock lock(Lock);

    if (BugOn(index >= Stdlib::ArraySize(CpuArray)))
        return;

    auto& cpu = CpuArray[index];
    cpu.SendIPISelf();
}

bool CpuTable::HasPerCpuTimer()
{
    return PerCpuTimer;
}

void CpuTable::SetPerCpuTimer()
{
    PerCpuTimer = true;
}

ulong CpuTable::GetRunningCpusNoLock()
{
    return (ulong)RunningMaskCached.Get();
}

void CpuTable::NoteCpuRunning(ulong index)
{
    if (index >= MaxCpus)
        return;

    ulong bit = 1UL << index;
    for (;;)
    {
        ulong old = (ulong)RunningMaskCached.Get();
        if ((old & bit) != 0)
            return;

        if ((ulong)RunningMaskCached.Cmpxchg((long)(old | bit), (long)old) == old)
            return;
    }
}

ulong CpuTable::GetRunningCpus()
{
    Stdlib::AutoLock lock(Lock);

    ulong result = 0;
    for (ulong i = 0; i < Stdlib::ArraySize(CpuArray); i++)
    {
        auto& cpu = CpuArray[i];
        if (cpu.GetState() & Cpu::StateRunning)
            result |= 1UL << i;
    }

    RunningMaskCached.Set((long)result);
    return result;
}

void CpuTable::Reset()
{
    /* An exited task's last reference is dropped by its CPU's idle loop
       (TaskQueue::ReapExited), and once the halt has begun no CPU idles
       again: the others are parked and this one is on its way out. Dropped
       here instead, or everything that exited since each CPU last idled
       leaks -- the soft IRQ tasks among them, stopped moments before the
       halt. Before the table lock: freeing a task frees its stack, and the
       TLB shootdown for that reads the running CPUs under this lock. */
    for (ulong i = 0; i < Stdlib::ArraySize(CpuArray); i++)
        CpuArray[i].GetTaskQueue().ReapExited();

    Stdlib::AutoLock lock(Lock);

    for (ulong i = 0; i < Stdlib::ArraySize(CpuArray); i++)
    {
        auto& cpu = CpuArray[i];
        cpu.Reset();
    }
}

bool Cpu::Run(Task::Func func, void *ctx)
{
    char name[TaskNameLen];
    Stdlib::SnPrintf(name, sizeof(name), "idle%lu", Index);
    IdleTaskPtr = Mm::TAlloc<Task, Tag>(name);
    if (IdleTaskPtr == nullptr)
    {
        return false;
    }

    IdleTaskPtr->SetCpuAffinity(1UL << Index);
    IdleTaskPtr->SetIdle();

    return IdleTaskPtr->Run(TaskQueue, func, ctx);
}

}