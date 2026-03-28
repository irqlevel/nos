#include "gdt.h"
#include "asm.h"
#include "trace.h"

namespace Kernel
{


Gdt::Gdt()
{
    Trace(0, "Gdt 0x%p", this);

    Entry[1].SetValue(((u64)1<<43) | ((u64)1<<44) | ((u64)1<<47) | ((u64)1<<53));
}

void Gdt::Save()
{
    TableDesc desc = {
        .Limit = sizeof(Entry),
        .Base = reinterpret_cast<u64>(&Entry[0]),
    };

    LoadGdt(&desc);

    Base = desc.Base;
    Limit = desc.Limit;
}

Gdt::~Gdt()
{
}

}