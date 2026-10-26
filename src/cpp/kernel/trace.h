#pragma once

#include <kernel/time.h>

#include <lib/stdlib.h>
#include <lib/error.h>
#include <lib/ring_buffer.h>

namespace kernel
{

const int EXC_LL = 0;
const int ACPI_LL = 0;
const int CMD_LL = 0;
const int KBD_LL = 3;
const int USB_LL = 3;
const int PAGE_ALLOCATOR_LL = 4;
const int ALLOCATOR_LL = 4;
const int POOL_LL = 4;
const int LAPIC_LL = 0;
const int IO_APIC_LL = 0;
const int MM_IO_LL = 4;
const int TEST_LL = 3;

/* One line per entropy source per reseed, and a reseed is a rare event -- but
   a source that quietly stops answering is worth being able to see. */

/* Three lines per ping, on the normal path. A machine someone is checking
   with `ping -f`, or just a host that gets scanned, buries everything else
   in the dmesg ring and in the netconsole stream. The counters behind
   `icmpstat` already say how many arrived and how many were answered;
   `loglevel 3` brings the per-packet detail back. */
const int ICMP_LL = 3;

/* Task create/start/free/destroy. Six lines per task, and the multitasking
   self-test makes one task per CPU twice over: on a 20-CPU box that is a
   third of everything printed before the network exists, all of it competing
   for room in the netconsole ring with the lines someone actually needs.
   `loglevel 3` brings it back on a running kernel. */
const int TASK_LL = 3;

/* Loadable modules: what each import bound to and the permissions each
   segment got, a few lines per insmod. The load and unload themselves are
   always traced. */
const int MODULE_LL = 3;

/* The highest level any call site above uses, and the bound the shell's
   loglevel command accepts. Raise it together with a noisier call site. */
const int MAX_TRACE_LEVEL = 5;

class Tracer
{
public:
    static Tracer& get_instance()
    {
        static Tracer instance;

        return instance;
    }

    PRINTF_FORMAT(2, 3) void output(const char *fmt, ...);

    PRINTF_FORMAT(3, 4) void output(stdlib::Error& err, const char *fmt, ...);

    void set_level(int level);

    int get_level();

    void set_console_suppressed(bool suppressed);
    bool is_console_suppressed();

private:
    Tracer();
    virtual ~Tracer();
    Tracer(const Tracer& other) = delete;
    Tracer(Tracer&& other) = delete;
    Tracer& operator=(const Tracer& other) = delete;
    Tracer& operator=(Tracer&& other) = delete;

    /* Written from the shell on one CPU while every other CPU reads it in
       Trace(); a stale read costs at most one line. */
    volatile int level_;
    bool console_suppressed_;
};

}

#define trace(level, fmt, ...)                                      \
do {                                                                \
    auto& macro_tracer = kernel::Tracer::get_instance();             \
    if (unlikely((level) <= macro_tracer.get_level()))               \
    {                                                               \
        auto macro_time = kernel::get_boot_time();                    \
        macro_tracer.output("%lu:%lu.%06lu:%s(),%s,%lu: " fmt "\n",  \
            (ulong)(level), macro_time.get_secs(), macro_time.get_usecs(), \
            __func__, stdlib::truncate_file_name(__FILE__),           \
            (ulong)__LINE__, ##__VA_ARGS__);                        \
    }                                                               \
} while (false)

#define trace_error(err, fmt, ...)                                   \
do {                                                                \
    auto& macro_tracer = kernel::Tracer::get_instance();             \
    if (unlikely(0 <= macro_tracer.get_level()))                     \
    {                                                               \
        auto macro_time = kernel::get_boot_time();                    \
        macro_tracer.output("%lu:%lu.%06lu:%s(),%s,%lu: Error %lu at %s(),%s,%lu: " fmt "\n", \
            0UL, macro_time.get_secs(), macro_time.get_usecs(),       \
            __func__, stdlib::truncate_file_name(__FILE__),           \
            (ulong)__LINE__, (ulong)err.get_code(), err.get_func(), stdlib::truncate_file_name(err.get_file()),  \
            (ulong)err.get_line(), ##__VA_ARGS__);                                  \
    }                                                               \
} while (false)