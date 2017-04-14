#include "panic.h"
#include "preempt.h"

#include <drivers/vga.h>

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
    va_list args;
    va_start(args, fmt);

    auto& vga = VgaTerm::GetInstance();
    vga.SetColor(VgaTerm::ColorRed, VgaTerm::ColorBlack);
    vga.VPrintf(fmt, args);
    va_end(args);

    InterruptDisable();
    Hlt();
}

}