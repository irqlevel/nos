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

    /* Called from the NMI handler. Returns true when a panic asked for this
       CPU's stack, in which case it has been recorded and the caller must
       not go on to panic about the NMI itself. */
    bool CollectRemoteStack();

private:

    /* The CPU that wedged the machine is, by definition, one that stopped
       answering interrupts -- so an ordinary IPI never reaches it and its
       stack, the one worth having, is the one missing from the report. An
       NMI does reach it. Each CPU writes its own slot from inside the NMI
       handler and the panicking CPU prints them. */
    void CollectRemoteStacks();

    static const size_t RemoteFrameCount = 12;

    /* Kept in step with Kernel::MaxCpus by a static_assert in panic.cpp;
       including cpu.h here would be circular. */
    static const size_t RemoteCpuCount = 64;

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

    Atomic Collecting;
    ulong RemoteFrame[RemoteCpuCount][RemoteFrameCount];
    u8 RemoteCount[RemoteCpuCount];
    Atomic RemoteDone[RemoteCpuCount];
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
