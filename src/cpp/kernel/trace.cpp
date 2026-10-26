#include "trace.h"
#include "dmesg.h"
#include "parameters.h"

#include <hal/console.h>

/* The disk log is Rust (src/rust/block/src/disklog.rs). */
extern "C" {
void rust_netconsole_log(const char* line, unsigned long len);
void rust_disklog_log(const char* line);
}

namespace kernel
{

Tracer::Tracer()
    : level_(0)
    , console_suppressed_(false)
{
}

void Tracer::set_level(int level)
{
    level_ = level;
}

int Tracer::get_level()
{
    return level_;
}

void Tracer::set_console_suppressed(bool suppressed)
{
    console_suppressed_ = suppressed;
}

bool Tracer::is_console_suppressed()
{
    return console_suppressed_;
}

Tracer::~Tracer()
{
}

void Tracer::output(const char *fmt, ...)
{
    char msg[256];

    va_list args;
    va_start(args, fmt);
    int size = stdlib::vsn_printf(msg, sizeof(msg), fmt, args);
    va_end(args);

    /* A line longer than the buffer comes out truncated, with "..." where it
       was cut (vsn_printf), and is printed: dropping it lost exactly the
       reports worth reading, a Rust panic's message among them. Only a
       format string this code got wrong still returns -1, and even then
       whatever was formatted before it is printed if there is any. */
    if (size < 0 && msg[0] == '\0')
        return;

    Dmesg::get_instance().print_string(msg);

    /* Capture is a memcpy into a ring buffer -- the send happens later, on the
       netconsole task, so this stays safe in IRQ context. */
    rust_netconsole_log(msg, stdlib::str_len(msg));

    /* And to the disk area, if one was prepared. Unlike the netconsole this
       writes now, synchronously: it exists for the boot that stops before
       there is a task to drain anything. */
    rust_disklog_log(msg);

    if (!console_suppressed_)
    {
        hal::console_write(msg);
    }
}

}