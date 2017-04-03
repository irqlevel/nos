#pragma once

#include "stdlib.h"
#include "error.h"
#include "pit.h"

namespace Kernel
{

namespace Core
{

const int SPageAllocatorLL = 4;
const int SAllocatorLL = 4;
const int SPoolLL = 4;
const int SharedPtrLL = 5;
const int BtreeLL = 6;
const int KbdLL = 3;
const int ExcLL = 0;

class Tracer
{
public:
    static Tracer& GetInstance()
    {
        static Tracer instance;

        return instance;
    }

    void Output(const char *fmt, ...);

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
       tracer.Output("%u:%u.%u:%s(),%s,%u: " fmt "\n",              \
            (level), time.Secs, time.NanoSecs,                      \
            __func__, Shared::TruncateFileName(__FILE__),           \
            __LINE__, ##__VA_ARGS__);                               \
    }                                                               \
} while (false)

#define  TraceError(err)                                                     \
do {                                                                        \
    if (!err.Ok())                                                          \
    {                                                                       \
        Trace(0, "Error %u occured at %s():%s,%u",                          \
            err.GetCode(), err.GetFunc(), err.GetFile(), err.GetLine());    \
    }                                                                       \
} while (false)