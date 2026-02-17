#pragma once

#include "atomic.h"
#include "asm.h"

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
        Barrier();
    }

    void WriteEnd()
    {
        Barrier();
        Seq.Inc();
    }

    /* Reader: retry loop */
    long ReadBegin()
    {
        long s;
        do {
            s = Seq.Get();
        } while (s & 1); /* spin while writer is active (odd) */
        Barrier();
        return s;
    }

    bool ReadRetry(long start)
    {
        Barrier();
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
