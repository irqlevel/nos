#include "lockless_ring.h"
#include "panic.h"

#include <hal/barrier.h>

namespace Kernel
{

LocklessRing::LocklessRing()
    : Cells(nullptr)
    , Capacity(0)
    , Mask(0)
    , EnqueuePos(0)
    , DequeuePos(0)
{
}

bool LocklessRing::Setup(Cell* cells, ulong capacity)
{
    if (cells == nullptr || capacity == 0)
        return false;

    /* Power of two: the mask is what makes the position wrap for free. */
    if ((capacity & (capacity - 1)) != 0)
        return false;

    Cells = cells;
    Capacity = capacity;
    Mask = capacity - 1;

    /* Cell i starts holding sequence i: empty, and the next producer to
       claim position i will find seq == pos. */
    for (ulong i = 0; i < capacity; i++)
    {
        Cells[i].Data = nullptr;
        Cells[i].Seq.Set((long)i);
    }

    EnqueuePos.Set(0);
    DequeuePos.Set(0);
    return true;
}

bool LocklessRing::Enqueue(void* data)
{
    if (Cells == nullptr)
        return false;

    for (;;)
    {
        long pos = EnqueuePos.Get();
        Cell* cell = &Cells[(ulong)pos & Mask];
        long diff = cell->Seq.Get() - pos;

        if (diff == 0)
        {
            /* Claim the slot. Losing this race only means another producer
               got there first, so go around. */
            if (EnqueuePos.Cmpxchg(pos + 1, pos) != pos)
                continue;

            cell->Data = data;

            /* Publish: the data store must be visible before the sequence
               number that hands the cell to a consumer. */
            Hal::SmpWmb();
            cell->Seq.Set(pos + 1);
            return true;
        }

        if (diff < 0)
            return false; /* full: the cell still belongs to a lap behind */
    }
}

bool LocklessRing::Dequeue(void*& data)
{
    if (Cells == nullptr)
        return false;

    for (;;)
    {
        long pos = DequeuePos.Get();
        Cell* cell = &Cells[(ulong)pos & Mask];
        long diff = cell->Seq.Get() - (pos + 1);

        if (diff == 0)
        {
            if (DequeuePos.Cmpxchg(pos + 1, pos) != pos)
                continue;

            /* Consume: the sequence read above must not be reordered after
               the data read below. */
            Hal::SmpRmb();
            data = cell->Data;
            cell->Data = nullptr;

            /* Release the cell to the producer one lap ahead. */
            Hal::SmpWmb();
            cell->Seq.Set(pos + (long)Mask + 1);
            return true;
        }

        if (diff < 0)
            return false; /* empty */
    }
}

ulong LocklessRing::Count()
{
    long enq = EnqueuePos.Get();
    long deq = DequeuePos.Get();

    return (enq > deq) ? (ulong)(enq - deq) : 0;
}

}
