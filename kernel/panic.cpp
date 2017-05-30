#include "panic.h"
#include "preempt.h"
#include "asm.h"
#include "cpu.h"

namespace Kernel
{

Panicker::Panicker()
{
}

Panicker::~Panicker()
{
}

bool Panicker::IsActive()
{
    return (Active.Get() != 0) ? true : false;
}

void Panicker::DoPanic(const char *fmt, ...)
{
    if (Active.Cmpxchg(1, 0) == 0)
    {
        va_list args;

        va_start(args, fmt);
        Shared::VsnPrintf(Message, sizeof(Message), fmt, args);
        va_end(args);
    }

    PreemptDisable();
    InterruptDisable();

    Cpu& cpu = CpuTable::GetInstance().GetCurrentCpu();
    CpuTable::GetInstance().SendIPIAllExclude(cpu.GetIndex());

    Hlt();
}

}