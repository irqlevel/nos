#include "cpu.h"
#include "interrupt.h"
#include "panic.h"
#include "trace.h"
#include "watchdog.h"
#include "timer.h"

#include <boot/boot64.h>

#include <drivers/lapic.h>
#include <drivers/pit.h>
#include <mm/new.h>
#include <mm/page_table.h>

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

    if (BugOn(!IsInterruptEnabled()))
        return;

    Hlt();
}

ulong Cpu::GetState()
{
    Stdlib::AutoLock lock(Lock);
    return State;
}

void Cpu::SetRunning()
{
    Stdlib::AutoLock lock(Lock);
    if (BugOn(State & StateRunning))
        return;

    State |= StateRunning;
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

bool CpuTable::SetBspIndex(ulong index)
{
    Stdlib::AutoLock lock(Lock);

    if (BugOn(index >= Stdlib::ArraySize(CpuArray)))
        return false;

    auto& cpu = CpuArray[BspIndex];
    if (BugOn(!(cpu.GetState() & Cpu::StateInited)))
        return false;

    cpu.SetRunning();
    BspIndex = index;
    return true;
}

bool CpuTable::StartAll()
{
    ulong startupCode = (ulong)ApStart16;

    Trace(0, "Starting cpus, startupCode 0x%p", startupCode);

    if (startupCode & (Const::PageSize - 1))
        return false;

    if (startupCode >= 0x100000)
        return false;

    {
        Stdlib::AutoLock lock(Lock);
        for (ulong index = 0; index < Stdlib::ArraySize(CpuArray); index++)
        {
            if (index != GetBspIndexLockHeld() && (CpuArray[index].GetState() & Cpu::StateInited))
            {
                Lapic::SendInit(index);
            }
        }
    }

    Pit::GetInstance().Wait(10 * Const::NanoSecsInMs); // 10ms

    {
        Stdlib::AutoLock lock(Lock);
        for (ulong index = 0; index < Stdlib::ArraySize(CpuArray); index++)
        {
            if (index != GetBspIndexLockHeld() && (CpuArray[index].GetState() & Cpu::StateInited))
            {
                Lapic::SendStartup(index, startupCode >> Const::PageShift);
            }
        }
    }

    Pit::GetInstance().Wait(100 * Const::NanoSecsInMs); // 100ms

    {
        Stdlib::AutoLock lock(Lock);
        for (ulong index = 0; index < Stdlib::ArraySize(CpuArray); index++)
        {
            if (index != GetBspIndexLockHeld() && (CpuArray[index].GetState() & Cpu::StateInited))
            {
                if (!(CpuArray[index].GetState() & Cpu::StateRunning))
                {
                    Trace(0, "Cpu %u still not running", index);
                    return false;
                }
            }
        }
    }

    Trace(0, "Cpus started");

    return true;
}

ulong CpuTable::GetCurrentCpuId()
{
    return Lapic::GetApicId();
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
    for (ulong i = 0; i < MaxCpus; i++)
    {
        if (cpuMask & (1UL << i))
            CpuArray[i].QueueIPITaskAsync(tasks[i]);
    }

    for (ulong i = 0; i < MaxCpus; i++)
    {
        if (cpuMask & (1UL << i))
            tasks[i].Completion.Wait();
        else
            tasks[i].Completion.Done();
    }
}

void CpuTable::InvalidateTlbAll()
{
    Mm::PageTable::InvalidateLocalTlb();

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
        if (exit)
            State |= StateExited;
    }

    if (exit)
    {
        Trace(0, "Cpu %u exited, state 0x%p, IPI count %u",
            Index, State, IPIConter.Get());

        InterruptDisable();
        Hlt();
        return;
    }

    Watchdog::GetInstance().Check();

    if (Index == 0)
    {
        TimerTable::GetInstance().ProcessTimers();
    }

    Lapic::EOI(CpuTable::IPIVector);

    Schedule();
}

void Cpu::QueueIPITaskAsync(IPITask& task)
{
    ulong flags = IPITaskLock.LockIrqSave();
    IPITaskList.InsertTail(&task.ListEntry);
    IPITaskLock.UnlockIrqRestore(flags);

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

void Cpu::SendIPISelf()
{
    Stdlib::AutoLock lock(Lock);

    if (BugOn(!(State & Cpu::StateRunning)))
        return;

    if (State & Cpu::StateExited)
        return;

    Lapic::SendIPI(Index, CpuTable::IPIVector);
}

void CpuTable::SendIPI(ulong index)
{
    Stdlib::AutoLock lock(Lock);

    if (BugOn(index >= Stdlib::ArraySize(CpuArray)))
        return;

    auto& cpu = CpuArray[index];
    cpu.SendIPISelf();
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

    return result;
}

void CpuTable::Reset()
{
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

    return IdleTaskPtr->Run(TaskQueue, func, ctx);
}

}