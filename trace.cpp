#include "trace.h"
#include "serial.h"
#include "dmesg.h"

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
    char msg[256];

    va_list args;
    va_start(args, fmt);
    int size = Shared::VsnPrintf(msg, sizeof(msg), fmt, args);
    va_end(args);

    if (size < 0)
        return;

    Serial::GetInstance().PrintString(msg);
    Dmesg::GetInstance().PrintString(msg);
}

}
}