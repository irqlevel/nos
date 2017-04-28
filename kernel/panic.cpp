#include "panic.h"
#include "preempt.h"
#include "asm.h"

#include <drivers/serial.h>

namespace Kernel
{

Panicker::Panicker()
{
}

Panicker::~Panicker()
{
}

void Panicker::DoPanic(const char *fmt, ...)
{
    InterruptDisable();

    va_list args;
    va_start(args, fmt);
    Serial::GetInstance().VPrintf(fmt, args);
    va_end(args);

    Hlt();
}

}