#pragma once

#include "stdlib.h"
#include "error.h"

namespace Kernel
{

namespace Core
{

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
       tracer.Output("%u:%s():%s,%u: " fmt "\n",                    \
            (level), __func__, Shared::TruncateFileName(__FILE__),  \
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