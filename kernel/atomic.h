#pragma once

#include <lib/stdlib.h>

namespace Kernel
{

class Atomic final
{
public:
    Atomic();
    Atomic(long value);
    void Inc();
    void Dec();
    bool DecAndTest();
    long Get();
    void Set(long value);
    void SetBit(ulong bit);
    bool TestBit(ulong bit);

    long Cmpxchg(long exchange, long comparand);

    ~Atomic();

    Atomic& operator=(Atomic&& other);
    Atomic(Atomic&& other);

private:
    Atomic(const Atomic& other) = delete;
    Atomic& operator=(const Atomic& other) = delete;

    volatile long Value;
};

static_assert(sizeof(Atomic) == sizeof(long), "Invalid size");

}
