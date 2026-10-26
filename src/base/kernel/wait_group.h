#pragma once

#include "atomic.h"

namespace kernel
{

class WaitGroup final
{
public:
    WaitGroup();
    WaitGroup(long count);
    ~WaitGroup();

    void add(long delta);
    void done();
    void wait();

    long get_counter();

private:
    WaitGroup(const WaitGroup& other) = delete;
    WaitGroup(WaitGroup&& other) = delete;
    WaitGroup& operator=(const WaitGroup& other) = delete;
    WaitGroup& operator=(WaitGroup&& other) = delete;

    Atomic counter_;
};

}
