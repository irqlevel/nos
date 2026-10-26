#pragma once

#include "atomic.h"
#include <hal/barrier.h>

namespace kernel
{

class SeqLock final
{
public:
    SeqLock()
        : seq_(0)
    {
    }

    ~SeqLock() {}

    /* Writer: call from single writer context (e.g. IRQ handler) */
    void write_begin()
    {
        seq_.inc();
        hal::smp_wmb();
    }

    void write_end()
    {
        hal::smp_wmb();
        seq_.inc();
    }

    /* Reader: retry loop */
    long read_begin()
    {
        long s;
        do {
            s = seq_.get();
        } while (s & 1); /* spin while writer is active (odd) */
        hal::smp_rmb();
        return s;
    }

    bool read_retry(long start)
    {
        hal::smp_rmb();
        return seq_.get() != start;
    }

private:
    SeqLock(const SeqLock& other) = delete;
    SeqLock(SeqLock&& other) = delete;
    SeqLock& operator=(const SeqLock& other) = delete;
    SeqLock& operator=(SeqLock&& other) = delete;

    Atomic seq_;
};

}
