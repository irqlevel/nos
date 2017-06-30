#include "cpu.h"
#include "panic.h"
#include "trace.h"
#include "watchdog.h"
#include "timer.h"

#include <boot/boot64.h>

#include <drivers/lapic.h>
#include <drivers/pit.h>

namespace Kernel
{

Cpu::Cpu()
    : Index(0)
    , State(0)
    , Task(nullptr)
{
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
    Shared::AutoLock lock(Lock);
    return State;
}

void Cpu::SetRunning()
{
    Shared::AutoLock lock(Lock);
    if (BugOn(State & StateRunning))
        return;

    State |= StateRunning;
}

void Cpu::SetExiting()
{
    Shared::AutoLock lock(Lock);
    State |= StateExiting;
}

void Cpu::Init(ulong index)
{
    Shared::AutoLock lock(Lock);
    if (BugOn(State & StateInited))
        return;

    Index = index;
    State |= StateInited;

    Trace(0, "Cpu 0x%p %u inited", this, Index);
}

Cpu::~Cpu()
{
    TaskQueue.Clear();

    if (Task != nullptr)
    {
        Task->Put();
        Task = nullptr;
    }
}

CpuTable::CpuTable()
    : BspIndex(0)
{
}

CpuTable::~CpuTable()
{
}

bool CpuTable::InsertCpu(ulong index)
{
    Shared::AutoLock lock(Lock);
    if (index >= Shared::ArraySize(CpuArray))
        return false;

    auto& cpu = CpuArray[index];
    if (cpu.GetState() & Cpu::StateInited)
        return false;

    cpu.Init(index);
    return true;
}

Cpu& CpuTable::GetCpu(ulong index)
{
    BugOn(index >= Shared::ArraySize(CpuArray));
    Cpu& cpu = CpuArray[index];
    return cpu;
}

ulong CpuTable::GetBspIndex()
{
    Shared::AutoLock lock(Lock);
    return GetBspIndexLockHeld();
}

ulong CpuTable::GetBspIndexLockHeld()
{
    return BspIndex;
}

bool CpuTable::SetBspIndex(ulong index)
{
    Shared::AutoLock lock(Lock);

    if (BugOn(index >= Shared::ArraySize(CpuArray)))
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

    if (startupCode & (Shared::PageSize - 1))
        return false;

    if (startupCode >= 0x100000)
        return false;

    {
        Shared::AutoLock lock(Lock);
        for (ulong index = 0; index < Shared::ArraySize(CpuArray); index++)
        {
            if (index != GetBspIndexLockHeld() && (CpuArray[index].GetState() & Cpu::StateInited))
            {
                Lapic::SendInit(index);
            }
        }
    }

    Pit::GetInstance().Wait(10 * Shared::NanoSecsInMs); // 10ms

    {
        Shared::AutoLock lock(Lock);
        for (ulong index = 0; index < Shared::ArraySize(CpuArray); index++)
        {
            if (index != GetBspIndexLockHeld() && (CpuArray[index].GetState() & Cpu::StateInited))
            {
                Lapic::SendStartup(index, startupCode >> Shared::PageShift);
            }
        }
    }

    Pit::GetInstance().Wait(100 * Shared::NanoSecsInMs); // 100ms

    {
        Shared::AutoLock lock(Lock);
        for (ulong index = 0; index < Shared::ArraySize(CpuArray); index++)
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
    for (ulong i = 0; i < 8 * sizeof(ulong); i++)
    {
        if ((cpuMask & ((ulong)1 << i)) && (i != self.GetIndex()))
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
    for (ulong i = 0; i < 8 * sizeof(ulong); i++)
    {
        if ((cpuMask & ((ulong)1 << i)) && (i != excludeIndex))
        {
            auto& cpu = GetCpu(i);

            cpu.SendIPISelf();
        }
    }
}

void CpuTable::SendIPIAll()
{
    ulong cpuMask = GetRunningCpus();
    for (ulong i = 0; i < 8 * sizeof(ulong); i++)
    {
        if ((cpuMask & ((ulong)1 << i)))
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

void Cpu::IPI(Context* ctx)
{
    (void)ctx;
    IPIConter.Inc();

    if (Panicker::GetInstance().IsActive())
    {
        OnPanic();
        return;
    }

    bool exit;
    {
        Shared::AutoLock lock(Lock);
        exit = (State & StateExiting) ? true : false;
        if (exit)
            State |= StateExited;
    }

    if (exit)
    {
        Trace(0, "Cpu %u exited, state 0x%p, IPI count %u",
            Index, State, IPIConter.Get());

        InterruptDisable();
        for (;;)
        {
            Pause();
        }
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

TaskQueue& Cpu::GetTaskQueue()
{
    return TaskQueue;
}

extern "C" void IPInterrupt(Context* ctx)
{
    auto& cpu = CpuTable::GetInstance().GetCurrentCpu();
    cpu.IPI(ctx);
}

void Cpu::SendIPISelf()
{
    Shared::AutoLock lock(Lock);

    if (BugOn(!(State & Cpu::StateRunning)))
        return;

    if (State & Cpu::StateExited)
        return;

    Lapic::SendIPI(Index, CpuTable::IPIVector);
}

void CpuTable::SendIPI(ulong index)
{
    Shared::AutoLock lock(Lock);

    if (BugOn(index >= Shared::ArraySize(CpuArray)))
        return;

    auto& cpu = CpuArray[index];
    cpu.SendIPISelf();
}

ulong CpuTable::GetRunningCpus()
{
    Shared::AutoLock lock(Lock);

    ulong result = 0;
    for (ulong i = 0; i < Shared::ArraySize(CpuArray); i++)
    {
        auto& cpu = CpuArray[i];
        if (cpu.GetState() & Cpu::StateRunning)
            result |= (ulong)1 << i;
    }

    return result;
}

bool Cpu::Run(Task::Func func, void *ctx)
{
    Task = new class Task("idle%u", Index);
    if (Task == nullptr)
    {
        return false;
    }

    Task->SetCpuAffinity((ulong)1 << Index);

    return Task->Run(TaskQueue, func, ctx);
}

}