#include "error.h"
#include "btree.h"
#include "trace.h"
#include "vector.h"

namespace Kernel
{

namespace Core
{

const int TestLL = 3;

Shared::Error TestBtree()
{
    Shared::Error err;

    Trace(TestLL, "TestBtree: started");

    size_t keyCount = 411;

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
        Trace(0, "TestBtree: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    for (size_t i = 0; i < keyCount; i++)
    {
        if (!tree.Insert(key[pos[i]], value[pos[i]]))
        {
            Trace(0, "TestBtree: cant insert key %llu", key[pos[i]]);
            return MakeError(Shared::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(0, "TestBtree: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    for (size_t i = 0; i < keyCount; i++)
    {
        bool exist;
        auto foundValue = tree.Lookup(key[pos[i]], exist);
        if (!exist)
        {
            Trace(0, "TestBtree: cant find key");
            return MakeError(Shared::Error::Unsuccessful);
        }

        if (foundValue != value[pos[i]])
        {
            Trace(0, "TestBtree: unexpected found value");
            return MakeError(Shared::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(0, "TestBtree: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    for (size_t i = 0; i < keyCount / 2; i++)
    {
        if (!tree.Delete(key[pos[i]]))
        {
            Trace(0, "TestBtree: cant delete key[%lu][%lu]=%llu", i, pos[i], key[pos[i]]);
            return MakeError(Shared::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(0, "TestBtree: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    for (size_t i = keyCount / 2; i < keyCount; i++)
    {
        bool exist;
        auto foundValue = tree.Lookup(key[pos[i]], exist);
        if (!exist)
        {
            Trace(0, "TestBtree: cant find key");
            return MakeError(Shared::Error::Unsuccessful);
        }

        if (foundValue != value[pos[i]])
        {
            Trace(0, "TestBtree: unexpected found value");
            return MakeError(Shared::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(0, "TestBtree: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    for (size_t i = keyCount / 2; i < keyCount; i++)
    {
        if (!tree.Delete(key[pos[i]]))
        {
            Trace(0, "TestBtree: cant delete key");
            return MakeError(Shared::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(0, "TestBtree: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    for (size_t i = 0; i < keyCount; i++)
    {
        bool exist;
        tree.Lookup(key[pos[i]], exist);
        if (exist)
        {
            Trace(0, "TestBtree: key still exist");
            return MakeError(Shared::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(0, "TestBtree: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    for (size_t i = 0; i < keyCount; i++)
    {
        if (!tree.Insert(key[pos[i]], value[pos[i]]))
        {
            Trace(0, "TestBtree: can't insert key'");
            return MakeError(Shared::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(0, "TestBtree: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    Trace(TestLL, "TestBtree: min depth %d max depth %d", tree.MinDepth(), tree.MaxDepth());

    tree.Clear();
    if (!tree.Check())
    {
        Trace(0, "TestBtree: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    Trace(TestLL, "TestBtree: complete");

    return MakeError(Shared::Error::Success);
}

Shared::Error Test()
{
    Shared::Error err;

    int *a = new int;
    *a = 1;
    Trace(0, "a = %p", a);
    delete a;

/*
    auto err = TestBtree();
    if (!err.Ok())
        return err;
*/
    return err;
}

}
}