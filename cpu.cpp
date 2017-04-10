#include "cpu.h"
#include "panic.h"
#include "lapic.h"
#include "trace.h"
#include "pit.h"

namespace Kernel
{

namespace Core
{

Cpu::Cpu()
    : Index(0)
    , State(0)
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

    if (BugOn(!(GetRflags() & 0x200))) //check interrupt flag is on
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

void Cpu::Init(ulong index)
{
    Shared::AutoLock lock(Lock);
    if (BugOn(State & StateInited))
        return;

    Index = index;
    State |= StateInited;
}

Cpu::~Cpu()
{
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
    Shared::AutoLock lock(Lock);

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
    Trace(0, "Starting cpus");

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

    Pit::GetInstance().Wait(10 * 1000000); // 10ms

    {
        Shared::AutoLock lock(Lock);
        for (ulong index = 0; index < Shared::ArraySize(CpuArray); index++)
        {
            if (index != GetBspIndexLockHeld() && (CpuArray[index].GetState() & Cpu::StateInited))
            {
                Lapic::SendStartup(index, 0x6); //0x6 page number = 0x6000 ap trampoline code, see boot64.asm
            }
        }
    }

    Pit::GetInstance().Wait(100 * 1000000); // 100ms

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

void Cpu::IPI(Context* ctx)
{
    Trace(0, "IPI cpu %u rip 0x%p", Index, ctx->GetRetRip());
    Lapic::EOI(CpuTable::IPIVector);
}

extern "C" void IPInterrupt(Context* ctx)
{
    auto& cpu = CpuTable::GetInstance().GetCurrentCpu();
    cpu.IPI(ctx);
}

void CpuTable::SendIPI(ulong index)
{
    Shared::AutoLock lock(Lock);

    if (BugOn(index >= Shared::ArraySize(CpuArray)))
        return;

    auto& cpu = CpuArray[BspIndex];
    if (BugOn(!(cpu.GetState() & Cpu::StateRunning)))
        return;

    Lapic::SendIPI(index, CpuTable::IPIVector);
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

}
}