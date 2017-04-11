#include "panic.h"
#include "debug.h"

#include <drivers/vga.h>

namespace Kernel
{

namespace Core
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

    auto& term = VgaTerm::GetInstance();
    term.SetColor(VgaTerm::ColorRed, VgaTerm::ColorBlack);
    term.Vprintf(fmt, args);
    va_end(args);

    DebugWait();
}

}
}