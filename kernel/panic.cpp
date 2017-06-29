#include "panic.h"
#include "preempt.h"
#include "asm.h"
#include "cpu.h"
#include "parameters.h"

#include <drivers/vga.h>

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
    bool first = false;

    if (Active.Cmpxchg(1, 0) == 0)
    {
        va_list args;

        va_start(args, fmt);
        Shared::VsnPrintf(Message, sizeof(Message), fmt, args);
        va_end(args);
        first = true;
    }

    PreemptDisable();
    InterruptDisable();

    Cpu& cpu = CpuTable::GetInstance().GetCurrentCpu();
    CpuTable::GetInstance().SendIPIAllExclude(cpu.GetIndex());

    if (first && Parameters::GetInstance().IsPanicVga())
    {
        VgaTerm::GetInstance().PrintString(Message);
    }

    Hlt();
}

}