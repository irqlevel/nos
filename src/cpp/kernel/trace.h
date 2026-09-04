#pragma once

#include <kernel/time.h>

#include <lib/stdlib.h>
#include <lib/error.h>
#include <lib/ring_buffer.h>

namespace Kernel
{

const int ExcLL = 0;
const int AcpiLL = 0;
const int CmdLL = 0;
const int KbdLL = 3;
const int UsbLL = 3;
const int PageAllocatorLL = 4;
const int AllocatorLL = 4;
const int PoolLL = 4;
const int LapicLL = 0;
const int IoApicLL = 0;
const int MmIoLL = 4;
const int TestLL = 3;

/* Three lines per ping, on the normal path. A machine someone is checking
   with `ping -f`, or just a host that gets scanned, buries everything else
   in the dmesg ring and in the netconsole stream. The counters behind
   `icmpstat` already say how many arrived and how many were answered;
   `loglevel 3` brings the per-packet detail back. */
const int IcmpLL = 3;

/* Task create/start/free/destroy. Six lines per task, and the multitasking
   self-test makes one task per CPU twice over: on a 20-CPU box that is a
   third of everything printed before the network exists, all of it competing
   for room in the netconsole ring with the lines someone actually needs.
   `loglevel 3` brings it back on a running kernel. */
const int TaskLL = 3;

/* The highest level any call site above uses, and the bound the shell's
   loglevel command accepts. Raise it together with a noisier call site. */
const int MaxTraceLevel = 5;

class Tracer
{
public:
    static Tracer& GetInstance()
    {
        static Tracer Instance;

        return Instance;
    }

    void Output(const char *fmt, ...);

    void Output(Stdlib::Error& err, const char *fmt, ...);

    void SetLevel(int level);

    int GetLevel();

    void SetConsoleSuppressed(bool suppressed);
    bool IsConsoleSuppressed();

private:
    Tracer();
    virtual ~Tracer();
    Tracer(const Tracer& other) = delete;
    Tracer(Tracer&& other) = delete;
    Tracer& operator=(const Tracer& other) = delete;
    Tracer& operator=(Tracer&& other) = delete;

    /* Written from the shell on one CPU while every other CPU reads it in
       Trace(); a stale read costs at most one line. */
    volatile int Level;
    bool ConsoleSuppressed;
};

}

#define Trace(level, fmt, ...)                                      \
do {                                                                \
    auto& tracer = Kernel::Tracer::GetInstance();                   \
    if (unlikely((level) <= tracer.GetLevel()))                     \
    {                                                               \
        auto time = Kernel::GetBootTime();                          \
        tracer.Output("%u:%u.%06u:%s(),%s,%u: " fmt "\n",            \
            (level), time.GetSecs(), time.GetUsecs(),               \
            __func__, Stdlib::TruncateFileName(__FILE__),           \
            (ulong)__LINE__, ##__VA_ARGS__);                        \
    }                                                               \
} while (false)

#define TraceError(err, fmt, ...)                                   \
do {                                                                \
    auto& tracer = Kernel::Tracer::GetInstance();                   \
    if (unlikely(0 <= tracer.GetLevel()))                           \
    {                                                               \
        auto time = Kernel::GetBootTime();                          \
        tracer.Output("%u:%u.%06u:%s(),%s,%u: Error %u at %s(),%s,%u: " fmt "\n",   \
            0, time.GetSecs(), time.GetUsecs(),                     \
            __func__, Stdlib::TruncateFileName(__FILE__),           \
            (ulong)__LINE__, (ulong)err.GetCode(), err.GetFunc(), Stdlib::TruncateFileName(err.GetFile()),  \
            (ulong)err.GetLine(), ##__VA_ARGS__);                                  \
    }                                                               \
} while (false)