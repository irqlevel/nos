#include "error.h"
#include "btree.h"
#include "trace.h"
#include "vector.h"
#include "gdt.h"
#include "cpu_state.h"

namespace Kernel
{

namespace Core
{

Shared::Error TestBtree()
{
    Shared::Error err;

    Trace(1, "TestBtree: started");

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

    Trace(1, "TestBtree: min depth %d max depth %d", tree.MinDepth(), tree.MaxDepth());

    tree.Clear();
    if (!tree.Check())
    {
        Trace(0, "TestBtree: check failed");
        return MakeError(Shared::Error::Unsuccessful);
    }

    Trace(1, "TestBtree: complete");

    return MakeError(Shared::Error::Success);
}

Shared::Error TestGdt()
{
    Gdt gdt;

    gdt.Load();

    Trace(0, "Gdt base 0x%p limit 0x%p", (ulong)gdt.GetBase(), (ulong)gdt.GetLimit());

    for (u16 selector = 0; selector < gdt.GetLimit(); selector+= 8)
    {
        GdtDescriptor desc = gdt.LoadDescriptor(selector);
        if (desc.GetValue() == 0)
            continue;

        Trace(0, "Gdt[0x%p] desc 0x%p limit 0x%p access 0x%p flag 0x%p",
            (ulong)selector, (ulong)desc.GetBase(), (ulong)desc.GetLimit(),
            (ulong)desc.GetAccess(), (ulong)desc.GetFlag());

    }

    return MakeError(Shared::Error::Success);
}

Shared::Error TestCpuState()
{
    CpuState cpu;

    cpu.Load();

    Trace(0, "Cpu cr0 0x%p cr1 0x%p cr2 0x%p cr3 0x%p cr4 0x%p",
        cpu.GetCr0(), cpu.GetCr1(), cpu.GetCr2(), cpu.GetCr3(),
        cpu.GetCr4());

    Trace(0, "Cpu eflags 0x%p sp 0x%p",
        cpu.GetEflags(), cpu.GetEsp());

    Trace(0, "Cpu ss 0x%p cs 0x%p ds 0x%p gs 0x%p fs 0x%p es 0x%p",
        (ulong)cpu.GetSs(), (ulong)cpu.GetCs(), (ulong)cpu.GetDs(),
        (ulong)cpu.GetGs(), (ulong)cpu.GetFs(), (ulong)cpu.GetEs());

    return MakeError(Shared::Error::Success);
}

Shared::Error Test()
{
    auto err = TestBtree();
    if (!err.Ok())
        return err;

    err = TestGdt();
    if (!err.Ok())
        return err;

    err = TestCpuState();
    if (!err.Ok())
        return err;

    return err;
}

}
}