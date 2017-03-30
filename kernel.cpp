#include "vga.h"
#include "trace.h"
#include "new.h"
#include "panic.h"
#include "debug.h"
#include "unique_ptr.h"
#include "sallocator.h"
#include "spage_allocator.h"
#include "atomic.h"
#include "shared_ptr.h"
#include "btree.h"
#include "error.h"
#include "vector.h"
#include "gdt.h"
#include "cpu_state.h"

using namespace Kernel::Core;
using namespace Shared;

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

extern "C" void kernel_main(void)
{
    Tracer::GetInstance().SetLevel(1);

    Trace(0, "Enter");

    auto err = Test();
    TraceError(err);

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

    CpuState cpuState;
    cpuState.Load();

    Trace(0, "Cpu cr0 0x%p cr1 0x%p cr2 0x%p cr3 0x%p cr4 0x%p",
        cpuState.GetCr0(), cpuState.GetCr1(), cpuState.GetCr2(), cpuState.GetCr3(),
        cpuState.GetCr4());

    Trace(0, "Cpu eflags 0x%p sp 0x%p",
        cpuState.GetEflags(), cpuState.GetEsp());

    Trace(0, "Cpu ss 0x%p cs 0x%p ds 0x%p gs 0x%p fs 0x%p es 0x%p",
        (ulong)cpuState.GetSs(), (ulong)cpuState.GetCs(), (ulong)cpuState.GetDs(),
        (ulong)cpuState.GetGs(), (ulong)cpuState.GetFs(), (ulong)cpuState.GetEs());

    Trace(0, "Exit");
}