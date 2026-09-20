#include "trace.h"
#include "dmesg.h"
#include "parameters.h"

#include <hal/console.h>

/* The disk log is Rust (src/rust/block/src/disklog.rs). */
extern "C" {
void rust_netconsole_log(const char* line, unsigned long len);
void rust_disklog_log(const char* line);
}

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

    /* A line longer than the buffer comes out truncated, with "..." where it
       was cut (VsnPrintf), and is printed: dropping it lost exactly the
       reports worth reading, a Rust panic's message among them. Only a
       format string this code got wrong still returns -1, and even then
       whatever was formatted before it is printed if there is any. */
    if (size < 0 && msg[0] == '\0')
        return;

    Dmesg::GetInstance().PrintString(msg);

    /* Capture is a memcpy into a ring buffer -- the send happens later, on the
       netconsole task, so this stays safe in IRQ context. */
    rust_netconsole_log(msg, Stdlib::StrLen(msg));

    /* And to the disk area, if one was prepared. Unlike the netconsole this
       writes now, synchronously: it exists for the boot that stops before
       there is a task to drain anything. */
    rust_disklog_log(msg);

    if (!ConsoleSuppressed)
    {
        Hal::ConsoleWrite(msg);
    }
}

}