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
    (void)fmt;

    InterruptDisable();
    Hlt();
}

}