#include "trace.h"
#include "vga.h"

namespace Kernel
{

namespace Core
{

Tracer::Tracer()
    : Level(0)
{
}

void Tracer::SetLevel(int level)
{
    Level = level;
}

int Tracer::GetLevel()
{
    return Level;
}

Tracer::~Tracer()
{
}

void Tracer::Output(const char *fmt, ...)
{
    va_list args;
    va_start(args, fmt);

    auto& term = VgaTerm::GetInstance();
    term.SetColor(VgaTerm::ColorWhite, VgaTerm::ColorBlack);
    term.Vprintf(fmt, args);
    va_end(args);
}

}
}