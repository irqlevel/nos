#include "panic.h"
#include "preempt.h"
#include "asm.h"
#include "cpu.h"
#include "parameters.h"
#include "trace.h"

#include <drivers/vga.h>

namespace Kernel
{

Panicker::Panicker()
{
    Trace(0, "Panicker 0x%p", this);
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
        Stdlib::VsnPrintf(Message, sizeof(Message), fmt, args);
        va_end(args);
    }

    InterruptDisable();

    Cpu& cpu = CpuTable::GetInstance().GetCurrentCpu();
    CpuTable::GetInstance().SendIPIAllExclude(cpu.GetIndex());

    for (;;)
    {
        Pause();
    }
}

}