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
const int PageAllocatorLL = 4;
const int AllocatorLL = 4;
const int PoolLL = 4;
const int LapicLL = 0;
const int IoApicLL = 0;
const int MmIoLL = 4;
const int TestLL = 3;

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

    int Level;
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