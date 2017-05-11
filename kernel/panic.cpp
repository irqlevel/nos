#include "panic.h"
#include "preempt.h"
#include "asm.h"
#include "cpu.h"

namespace Kernel
{

Panicker::Panicker()
    : Active(false)
{
}

Panicker::~Panicker()
{
}

bool Panicker::IsActive()
{
    return Active;
}

void Panicker::DoPanic(const char *fmt, ...)
{
    (void)fmt;

    Active = true;

    PreemptDisable();
    InterruptDisable();

    Cpu& cpu = CpuTable::GetInstance().GetCurrentCpu();
    CpuTable::GetInstance().SendIPIAllExclude(cpu.GetIndex());

    Hlt();
}

}