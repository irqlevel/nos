#include "cpu.h"
#include "panic.h"

namespace Kernel
{

namespace Core
{

Cpu::Cpu()
    : Index(0)
    , Active(false)
{ 
}

ulong Cpu::GetIndex()
{
    return Index;
}

void Cpu::Idle()
{
    Hlt();
}

bool Cpu::IsActive()
{
    Shared::AutoLock lock(Lock);
    return Active;
}

bool Cpu::Activate(ulong index)
{
    Shared::AutoLock lock(Lock);
    Active = true;
    Index = index;
    return true;
}

Cpu::~Cpu()
{
}

CpuTable::CpuTable()
{
}

CpuTable::~CpuTable()
{
}

bool CpuTable::RegisterCpu(ulong index)
{
    Shared::AutoLock lock(Lock);
    if (index >= Shared::ArraySize(CpuArray))
        return false;

    auto& cpu = CpuArray[index];
    if (cpu.IsActive())
        return false;

    return cpu.Activate(index);
}

Cpu& CpuTable::GetCpu(ulong index)
{
    Shared::AutoLock lock(Lock);

    BugOn(index >= Shared::ArraySize(CpuArray));
    Cpu& cpu = CpuArray[index];
    BugOn(!cpu.IsActive());
    return cpu;
}

}
}