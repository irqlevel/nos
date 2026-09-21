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

    Hlt();
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

    Trace(0, "Cpu 0x%p %u inited", this, Index);
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

            while (!(cpu.GetState() & Cpu::StateExited))
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

/* Where an exiting CPU ends, for good. Once StateExited is visible the CPU
   that asked goes on to free the tasks, their stacks and at last the heap --
   the stack this one is standing on among them -- so this CPU must never go
   back to the task it interrupted, and StateExited is published only when
   there is nothing left for it to do. One halt is not enough to stay put:
   arm64's WFI may complete at any moment -- a pending interrupt ends it even
   while masked, and under HVF every AP was out of it within milliseconds --
   and x86's HLT ends at the first NMI. A CPU that fell out of it used to
   return from the IPI into a task whose stack was being freed under it: the
   data abort every arm64 poweroff printed from an AP, in the middle of the
   boot CPU's static destructors. */
void Cpu::Park()
{
    InterruptDisable();

    /* Before StateExited: from then on the stack under this CPU may be freed
       at any moment, and a panic must already know not to send it an NMI,
       whose delivery pushes a frame onto that stack (see
       Panicker::CollectRemoteStacks). */
    Parked.Set(1);

    {
        Stdlib::AutoLock lock(Lock);
        State |= StateExited;
    }

    for (;;)
    {
        Hlt();
    }
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
        if ((cpuMask & (1UL << i)) &&
            (CpuArray[i].GetState() & Cpu::StateExited))
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
        Panic("TLB shootdown: cpus 0x%p never acknowledged (asked 0x%p)",
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
        Trace(0, "Cpu %u exited, state 0x%p, IPI count %u",
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

/* This CPU's own periodic tick. One interrupt per CPU delivered by its own
   local APIC, instead of one interrupt on one CPU followed by an IPI to
   every other -- which cost a fan of ICR writes per tick and, worse, made
   the machine's entire sense of time depend on one CPU still answering
   interrupts. A CPU that wedges now stops only its own tick, and the others
   go on ticking and notice. */
void Cpu::TimerTick(Context* ctx)
{
    if (Panicker::GetInstance().IsActive())
    {
        Hal::IrqEoi(CpuTable::TimerVector);
        return;
    }

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

extern "C" void LapicTimerInterrupt(Context* ctx)
{
    InterruptStats::Inc(IrqLapicTimer);
    auto& cpu = CpuTable::GetInstance().GetCurrentCpu();
    cpu.TimerTick(ctx);
}

void Cpu::SendIPISelf()
{
    Stdlib::AutoLock lock(Lock);

    if (BugOn(!(State & Cpu::StateRunning)))
        return;

    if (State & Cpu::StateExited)
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
    IdleTaskPtr = Mm::TAlloc<Task, Tag>("idle%u", Index);
    if (IdleTaskPtr == nullptr)
    {
        return false;
    }

    IdleTaskPtr->SetCpuAffinity(1UL << Index);
    IdleTaskPtr->SetIdle();

    return IdleTaskPtr->Run(TaskQueue, func, ctx);
}

}