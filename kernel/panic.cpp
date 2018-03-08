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
    if (Active.Cmpxchg(1, 0) == 0)
    {
        va_list args;

        va_start(args, fmt);
        Stdlib::VsnPrintf(Message, sizeof(Message), fmt, args);
        va_end(args);

        //VgaTerm::GetInstance().PrintString(Message);
    }

    PreemptDisable();
    InterruptDisable();

/*
    Cpu& cpu = CpuTable::GetInstance().GetCurrentCpu();
    CpuTable::GetInstance().SendIPIAllExclude(cpu.GetIndex());
*/

    for (;;)
    {
        Pause();
    }
}

}