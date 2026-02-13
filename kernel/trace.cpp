#include "trace.h"
#include "dmesg.h"
#include "parameters.h"

#include <drivers/serial.h>
#include <drivers/vga.h>

namespace Kernel
{

Tracer::Tracer()
    : Level(0)
    , ConsoleSuppressed(false)
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

void Tracer::SetConsoleSuppressed(bool suppressed)
{
    ConsoleSuppressed = suppressed;
}

bool Tracer::IsConsoleSuppressed()
{
    return ConsoleSuppressed;
}

Tracer::~Tracer()
{
}

void Tracer::Output(const char *fmt, ...)
{
    char msg[256];

    va_list args;
    va_start(args, fmt);
    int size = Stdlib::VsnPrintf(msg, sizeof(msg), fmt, args);
    va_end(args);

    if (size < 0)
        return;

    Dmesg::GetInstance().PrintString(msg);

    if (!ConsoleSuppressed)
    {
        Serial::GetInstance().PrintString(msg);
        Serial::GetInstance().Flush();

        if (Parameters::GetInstance().IsTraceVga())
        {
            VgaTerm::GetInstance().PrintString(msg);
        }
    }
}

}