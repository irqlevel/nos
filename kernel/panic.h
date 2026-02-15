#pragma once

#include <lib/stdlib.h>
#include "atomic.h"

namespace Kernel
{

struct Context;

class Panicker
{
public:
    static Panicker& GetInstance()
    {
        static Panicker Instance;

        return Instance;
    }

    void DoPanic(const char *fmt, ...);

    void DoPanicCtx(Context* ctx, bool hasErrorCode, const char *fmt, ...);

    bool IsActive();

private:

    Panicker();
    virtual ~Panicker();
    Panicker(const Panicker& other) = delete;
    Panicker(Panicker&& other) = delete;
    Panicker& operator=(const Panicker& other) = delete;
    Panicker& operator=(Panicker&& other) = delete;

    void PrintOutput(const char* str);
    void DumpContext();
    void DumpBacktrace(ulong* frames, size_t count);

    char Message[256];
    Atomic Active;
};

}

#define Panic(fmt, ...)                                             \
do {                                                                \
    auto& panicker = Kernel::Panicker::GetInstance();               \
    panicker.DoPanic("PANIC:%s():%s,%u: " fmt "\n",                 \
        __func__, Stdlib::TruncateFileName(__FILE__),               \
        (ulong)__LINE__, ##__VA_ARGS__);                            \
} while (false)

#define PanicCtx(ctx, hasErrCode, fmt, ...)                         \
do {                                                                \
    auto& panicker = Kernel::Panicker::GetInstance();               \
    panicker.DoPanicCtx(ctx, hasErrCode,                            \
        "PANIC:%s():%s,%u: " fmt "\n",                              \
        __func__, Stdlib::TruncateFileName(__FILE__),               \
        (ulong)__LINE__, ##__VA_ARGS__);                            \
} while (false)

static inline bool DoBugOn(const char *func, const char *file, int line)
{
    auto& panicker = Kernel::Panicker::GetInstance();
    panicker.DoPanic("PANIC:%s():%s,%u: BUG\n", func, file, (ulong)line);
    return true;
}

#define BugOn(condition)    \
    (unlikely(condition)) ? DoBugOn(__func__, Stdlib::TruncateFileName(__FILE__), __LINE__) : \
    false
