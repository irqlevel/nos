#pragma once

#include "spin_lock.h"

namespace Kernel
{

namespace Core
{

class Atomic
{
public:
    Atomic();
    Atomic(int value);
    void Inc();
    bool DecAndTest();
    int Get();
    void Set(int value);
    ~Atomic();

    Atomic& operator=(Atomic&& other);
    Atomic(Atomic&& other);

private:
    Atomic(const Atomic& other) = delete;
    Atomic& operator=(const Atomic& other) = delete;

    int Value;
    SpinLock Lock;
};

}

}
