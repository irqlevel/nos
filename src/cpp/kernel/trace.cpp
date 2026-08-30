#include "trace.h"
#include "dmesg.h"
#include "parameters.h"

#include <hal/console.h>
#include <net/netconsole.h>

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

    /* Capture is a memcpy into a ring buffer -- the send happens later, on the
       netconsole task, so this stays safe in IRQ context. */
    Netconsole::GetInstance().Log(msg);

    if (!ConsoleSuppressed)
    {
        Hal::ConsoleWrite(msg);
    }
}

}