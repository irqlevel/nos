#include "error.h"
#include "btree.h"
#include "trace.h"
#include "vector.h"
#include "debug.h"
#include "stdlib.h"
#include "ring_buffer.h"

namespace Kernel
{

namespace Core
{

const int TestLL = 3;

Shared::Error TestBtree()
{
    Shared::Error err;

    Trace(TestLL, "TestBtree: started");

    size_t keyCount = 431;

    Vector<size_t> pos;
    if (!pos.ReserveAndUse(keyCount))
        return MakeError(Shared::Error::NoMemory);

    for (size_t i = 0; i < keyCount; i++)
    {
        pos[i] = i;
    }

    Vector<u32> key;
    if (!key.ReserveAndUse(keyCount))
        return MakeError(Shared::Error::NoMemory);
    for (size_t i = 0; i < keyCount; i++)
        key[i] = i;

    Vector<u32> value;
    if (!value.ReserveAndUse(keyCount))
        return MakeError(Shared::Error::NoMemory);

    for (size_t i = 0; i < keyCount; i++)
        value[i] = i;

    Btree<u32, u32, 4> tree;

    if (!tree.Check())
    {
        Trace(TestLL, "TestBtree: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    for (size_t i = 0; i < keyCount; i++)
    {
        if (!tree.Insert(key[pos[i]], value[pos[i]]))
        {
            Trace(TestLL, "TestBtree: cant insert key %llu", key[pos[i]]);
            return MakeError(Shared::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(TestLL, "TestBtree: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    for (size_t i = 0; i < keyCount; i++)
    {
        bool exist;
        auto foundValue = tree.Lookup(key[pos[i]], exist);
        if (!exist)
        {
            Trace(TestLL, "TestBtree: cant find key");
            return MakeError(Shared::Error::Unsuccessful);
        }

        if (foundValue != value[pos[i]])
        {
            Trace(TestLL, "TestBtree: unexpected found value");
            return MakeError(Shared::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(TestLL, "TestBtree: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    for (size_t i = 0; i < keyCount / 2; i++)
    {
        if (!tree.Delete(key[pos[i]]))
        {
            Trace(TestLL, "TestBtree: cant delete key[%lu][%lu]=%llu", i, pos[i], key[pos[i]]);
            return MakeError(Shared::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(TestLL, "TestBtree: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    for (size_t i = keyCount / 2; i < keyCount; i++)
    {
        bool exist;
        auto foundValue = tree.Lookup(key[pos[i]], exist);
        if (!exist)
        {
            Trace(TestLL, "TestBtree: cant find key");
            return MakeError(Shared::Error::Unsuccessful);
        }

        if (foundValue != value[pos[i]])
        {
            Trace(TestLL, "TestBtree: unexpected found value");
            return MakeError(Shared::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(TestLL, "TestBtree: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    for (size_t i = keyCount / 2; i < keyCount; i++)
    {
        if (!tree.Delete(key[pos[i]]))
        {
            Trace(TestLL, "TestBtree: cant delete key");
            return MakeError(Shared::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(TestLL, "TestBtree: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    for (size_t i = 0; i < keyCount; i++)
    {
        bool exist;
        tree.Lookup(key[pos[i]], exist);
        if (exist)
        {
            Trace(TestLL, "TestBtree: key still exist");
            return MakeError(Shared::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(TestLL, "TestBtree: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    for (size_t i = 0; i < keyCount; i++)
    {
        if (!tree.Insert(key[pos[i]], value[pos[i]]))
        {
            Trace(TestLL, "TestBtree: can't insert key'");
            return MakeError(Shared::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(TestLL, "TestBtree: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    Trace(TestLL, "TestBtree: min depth %d max depth %d", tree.MinDepth(), tree.MaxDepth());

    tree.Clear();
    if (!tree.Check())
    {
        Trace(TestLL, "TestBtree: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    Trace(TestLL, "TestBtree: complete");

    return MakeError(Shared::Error::Success);
}

Shared::Error TestAllocator()
{
    Shared::Error err;

    for (size_t size = 1; size <= Shared::PageSize; size++)
    {
        u8 *block = new u8[size];
        if (block == nullptr)
        {
            return Shared::Error::NoMemory;
        }

        block[0] = 1;
        block[size / 2] = 1;
        block[size - 1] = 1;
        delete [] block;
    }

    return MakeError(Shared::Error::Success);
}

Shared::Error TestRingBuffer()
{
    RingBuffer<u8, 3> rb;

    if (!rb.Put(0x1))
        return MakeError(Shared::Error::Unsuccessful);

    if (!rb.Put(0x2))
        return MakeError(Shared::Error::Unsuccessful);

    if (!rb.Put(0x3))
        return MakeError(Shared::Error::Unsuccessful);

    if (rb.Put(0x4))
        return MakeError(Shared::Error::Unsuccessful);

    if (!rb.IsFull())
        return MakeError(Shared::Error::Unsuccessful);

    if (rb.IsEmpty())
        return MakeError(Shared::Error::Unsuccessful);    

    if (rb.Get() != 0x1)
        return MakeError(Shared::Error::Unsuccessful);

    if (rb.Get() != 0x2)
        return MakeError(Shared::Error::Unsuccessful);

    if (rb.Get() != 0x3)
        return MakeError(Shared::Error::Unsuccessful);

    if (!rb.IsEmpty())
        return MakeError(Shared::Error::Unsuccessful);
   
    return MakeError(Shared::Error::Success);
}

Shared::Error Test()
{
    Shared::Error err;

    err = TestAllocator();
    if (!err.Ok())
        return err;

    err = TestBtree();
    if (!err.Ok())
        return err;

    err = TestRingBuffer();
    if (!err.Ok())
        return err;

    return err;
}

}
}
