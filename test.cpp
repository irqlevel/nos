#include "error.h"
#include "btree.h"
#include "trace.h"
#include "vector.h"

namespace Kernel
{

namespace Core
{

Shared::Error BtreeTest()
{
    Shared::Error err;

    Trace(1, "BtreeTest: started");

    size_t keyCount = 913;

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
        Trace(0, "BtreeTest: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    for (size_t i = 0; i < keyCount; i++)
    {
        if (!tree.Insert(key[pos[i]], value[pos[i]]))
        {
            Trace(0, "BtreeTest: cant insert key %llu", key[pos[i]]);
            return MakeError(Shared::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(0, "BtreeTest: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    for (size_t i = 0; i < keyCount; i++)
    {
        bool exist;
        auto foundValue = tree.Lookup(key[pos[i]], exist);
        if (!exist)
        {
            Trace(0, "BtreeTest: cant find key");
            return MakeError(Shared::Error::Unsuccessful);
        }

        if (foundValue != value[pos[i]])
        {
            Trace(0, "BtreeTest: unexpected found value");
            return MakeError(Shared::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(0, "BtreeTest: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    for (size_t i = 0; i < keyCount / 2; i++)
    {
        if (!tree.Delete(key[pos[i]]))
        {
            Trace(0, "BtreeTest: cant delete key[%lu][%lu]=%llu", i, pos[i], key[pos[i]]);
            return MakeError(Shared::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(0, "BtreeTest: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    for (size_t i = keyCount / 2; i < keyCount; i++)
    {
        bool exist;
        auto foundValue = tree.Lookup(key[pos[i]], exist);
        if (!exist)
        {
            Trace(0, "BtreeTest: cant find key");
            return MakeError(Shared::Error::Unsuccessful);
        }

        if (foundValue != value[pos[i]])
        {
            Trace(0, "BtreeTest: unexpected found value");
            return MakeError(Shared::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(0, "BtreeTest: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    for (size_t i = keyCount / 2; i < keyCount; i++)
    {
        if (!tree.Delete(key[pos[i]]))
        {
            Trace(0, "BtreeTest: cant delete key");
            return MakeError(Shared::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(0, "BtreeTest: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    for (size_t i = 0; i < keyCount; i++)
    {
        bool exist;
        tree.Lookup(key[pos[i]], exist);
        if (exist)
        {
            Trace(0, "BtreeTest: key still exist");
            return MakeError(Shared::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(0, "BtreeTest: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    for (size_t i = 0; i < keyCount; i++)
    {
        if (!tree.Insert(key[pos[i]], value[pos[i]]))
        {
            Trace(0, "BtreeTest: can't insert key'");
            return MakeError(Shared::Error::Unsuccessful);
        }
    }
    if (!tree.Check())
    {
        Trace(0, "BtreeTest: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    Trace(1, "BtreeTest: min depth %d max depth %d", tree.MinDepth(), tree.MaxDepth());

    tree.Clear();
    if (!tree.Check())
    {
        Trace(0, "BtreeTest: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    Trace(1, "BtreeTest: complete");

    return MakeError(Shared::Error::Success);
}

Shared::Error Test()
{
    auto err = BtreeTest();
    return err;
}

}
}