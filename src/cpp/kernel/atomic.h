#pragma once

#include <lib/stdlib.h>

namespace Kernel
{

class Atomic final
{
public:
    /* constexpr, and the destructor trivial: an Atomic with static storage
       is then initialised by the compiler rather than by a constructor the
       kernel would never run (no .init_array here; the link refuses one).
       Plain initialisation, not Set(): nothing can see the object before
       its constructor returns. */
    constexpr Atomic()
        : Value(0)
    {
    }

    constexpr Atomic(long value)
        : Value(value)
    {
    }
    void Inc();
    void Dec();
    void Add(long delta);
    bool DecAndTest();
    long Get();
    void Set(long value);
    bool SetBit(ulong bit);
    bool ClearBit(ulong bit);
    bool TestBit(ulong bit);

    long Cmpxchg(long exchange, long comparand);

    ~Atomic() = default;

    Atomic& operator=(Atomic&& other);
    Atomic(Atomic&& other);

private:
    Atomic(const Atomic& other) = delete;
    Atomic& operator=(const Atomic& other) = delete;

    volatile long Value;
};

static_assert(sizeof(Atomic) == sizeof(long), "Invalid size");

}
