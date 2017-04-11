#pragma once

#include <drivers/pit.h>

#include <lib/stdlib.h>
#include <lib/error.h>
#include <lib/ring_buffer.h>

namespace Kernel
{

namespace Core
{

const int ExcLL = 0;
const int AcpiLL = 0;
const int CmdLL = 0;
const int KbdLL = 3;
const int SPageAllocatorLL = 4;
const int SAllocatorLL = 4;
const int SPoolLL = 4;
const int SharedPtrLL = 5;
const int BtreeLL = 6;
const int LapicLL = 0;
const int IoApicLL = 0;
const int MmIoLL = 4;

class Tracer
{
public:
    static Tracer& GetInstance()
    {
        static Tracer instance;

        return instance;
    }

    void Output(const char *fmt, ...);

    void Output(Shared::Error& err, const char *fmt, ...);

    void SetLevel(int level);

    int GetLevel();

private:
    Tracer();
    virtual ~Tracer();
    Tracer(const Tracer& other) = delete;
    Tracer(Tracer&& other) = delete;
    Tracer& operator=(const Tracer& other) = delete;
    Tracer& operator=(Tracer&& other) = delete;

    int Level;
};

}

}

#define Trace(level, fmt, ...)                                      \
do {                                                                \
    auto& tracer = Kernel::Core::Tracer::GetInstance();             \
    if (unlikely((level) <= tracer.GetLevel()))                     \
    {                                                               \
        auto time = Pit::GetInstance().GetTime();                   \
        tracer.Output("%u:%u.%u:%s(),%s,%u: " fmt "\n",             \
            (level), time.Secs, time.NanoSecs,                      \
            __func__, Shared::TruncateFileName(__FILE__),           \
            (ulong)__LINE__, ##__VA_ARGS__);                               \
    }                                                               \
} while (false)

#define TraceError(err, fmt, ...)                                   \
do {                                                                \
    auto& tracer = Kernel::Core::Tracer::GetInstance();             \
    if (unlikely(0 <= tracer.GetLevel()))                           \
    {                                                               \
        auto time = Pit::GetInstance().GetTime();                   \
        tracer.Output("%u:%u.%u:%s(),%s,%u: Error %u at %s(),%s,%u: " fmt "\n",    \
            0, time.Secs, time.NanoSecs,                            \
            __func__, Shared::TruncateFileName(__FILE__),           \
            (ulong)__LINE__, (ulong)err.GetCode(), err.GetFunc(), Shared::TruncateFileName(err.GetFile()),  \
            (ulong)err.GetLine(), ##__VA_ARGS__);                                  \
    }                                                               \
} while (false)