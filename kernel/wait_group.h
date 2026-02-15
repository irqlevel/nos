#pragma once

#include "atomic.h"

namespace Kernel
{

class WaitGroup final
{
public:
    WaitGroup();
    WaitGroup(long count);
    ~WaitGroup();

    void Add(long delta);
    void Done();
    void Wait();

    long GetCounter();

private:
    WaitGroup(const WaitGroup& other) = delete;
    WaitGroup(WaitGroup&& other) = delete;
    WaitGroup& operator=(const WaitGroup& other) = delete;
    WaitGroup& operator=(WaitGroup&& other) = delete;

    Atomic Counter;
};

}
