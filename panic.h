#pragma once

#include "memory.h"

namespace Kernel
{

namespace Core
{

class Panicker
{
public:
    static Panicker& GetInstance()
    {
        static Panicker instance;

        return instance;
    }

    void DoPanic(const char *fmt, ...);

private:

    Panicker();
    virtual ~Panicker();
    Panicker(const Panicker& other) = delete;
    Panicker(Panicker&& other) = delete;
    Panicker& operator=(const Panicker& other) = delete;
    Panicker& operator=(Panicker&& other) = delete;
};

}
}

#define Panic(fmt, ...)                                             \
do {                                                                \
    auto& panicker = Kernel::Core::Panicker::GetInstance();         \
    panicker.DoPanic("PANIC:%s():%s,%u: " fmt "\n",                 \
        __func__, Shared::TruncateFileName(__FILE__),               \
        __LINE__, ##__VA_ARGS__);                                   \
} while (false)

static inline bool DoBugOn(const char *func, const char *file, int line)
{
    auto& panicker = Kernel::Core::Panicker::GetInstance();
    panicker.DoPanic("PANIC:%s():%s,%u: BUG\n", func, file, line);
    return true;
}

#define BugOn(condition)    \
    (unlikely(condition)) ? DoBugOn(__func__, Shared::TruncateFileName(__FILE__), __LINE__) : \
    false
