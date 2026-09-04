#pragma once

#include <lib/stdlib.h>
#include "atomic.h"

namespace Kernel
{

/* Bounded multi-producer multi-consumer queue of pointers, one compare-and-
   swap per operation and no lock anywhere.
 
   The structure is Vyukov's: every cell carries its own sequence number, and
   a producer or consumer claims a slot by advancing a shared position with a
   single CAS. Sequence numbers only ever increase, so the tag that a
   pointer-based stack needs a 128-bit CAS to carry is here part of the value
   itself -- there is no ABA to have, and no cmpxchg16b or LDXP to write.
 
   Capacity must be a power of two. Enqueue fails when full and Dequeue when
   empty; neither ever blocks, which is what makes it safe to call from the
   places that must not block, including with interrupts off. */
class LocklessRing final
{
public:
    struct Cell
    {
        Atomic Seq;
        void* Data;
    };

    LocklessRing();

    /* cells must hold capacity entries and outlive the ring. */
    bool Setup(Cell* cells, ulong capacity);

    bool Enqueue(void* data);
    bool Dequeue(void*& data);

    /* A snapshot, immediately stale by construction: for reporting, never
       for a decision. */
    ulong Count();
    ulong GetCapacity() { return Capacity; }

private:
    LocklessRing(const LocklessRing& other) = delete;
    LocklessRing& operator=(const LocklessRing& other) = delete;

    Cell* Cells;
    ulong Capacity;
    ulong Mask;

    Atomic EnqueuePos;
    Atomic DequeuePos;
};

}
