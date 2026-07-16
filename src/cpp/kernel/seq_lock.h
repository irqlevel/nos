#pragma once

#include "atomic.h"
#include <hal/barrier.h>

namespace Kernel
{

class SeqLock final
{
public:
    SeqLock()
        : Seq(0)
    {
    }

    ~SeqLock() {}

    /* Writer: call from single writer context (e.g. IRQ handler) */
    void WriteBegin()
    {
        Seq.Inc();
        Hal::SmpWmb();
    }

    void WriteEnd()
    {
        Hal::SmpWmb();
        Seq.Inc();
    }

    /* Reader: retry loop */
    long ReadBegin()
    {
        long s;
        do {
            s = Seq.Get();
        } while (s & 1); /* spin while writer is active (odd) */
        Hal::SmpRmb();
        return s;
    }

    bool ReadRetry(long start)
    {
        Hal::SmpRmb();
        return Seq.Get() != start;
    }

private:
    SeqLock(const SeqLock& other) = delete;
    SeqLock(SeqLock&& other) = delete;
    SeqLock& operator=(const SeqLock& other) = delete;
    SeqLock& operator=(SeqLock&& other) = delete;

    Atomic Seq;
};

}
