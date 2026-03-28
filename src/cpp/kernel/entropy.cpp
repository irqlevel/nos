#include "entropy.h"

#include <kernel/trace.h>
#include <lib/stdlib.h>

namespace Kernel
{

EntropySourceTable::EntropySourceTable()
    : Count(0)
{
    for (ulong i = 0; i < MaxSources; i++)
        Sources[i] = nullptr;
}

EntropySourceTable::~EntropySourceTable()
{
}

bool EntropySourceTable::Register(EntropySource* src)
{
    if (Count >= MaxSources || src == nullptr)
        return false;

    Sources[Count] = src;
    Count++;

    Trace(0, "EntropySource registered: %s", src->GetName());
    return true;
}

EntropySource* EntropySourceTable::Find(const char* name)
{
    for (ulong i = 0; i < Count; i++)
    {
        if (Sources[i] && Stdlib::StrCmp(Sources[i]->GetName(), name) == 0)
            return Sources[i];
    }
    return nullptr;
}

EntropySource* EntropySourceTable::GetDefault()
{
    if (Count == 0)
        return nullptr;
    return Sources[0];
}

ulong EntropySourceTable::GetCount()
{
    return Count;
}

void EntropySourceTable::Dump(Stdlib::Printer& printer)
{
    if (Count == 0)
    {
        printer.Printf("no entropy sources\n");
        return;
    }

    for (ulong i = 0; i < Count; i++)
    {
        if (!Sources[i])
            continue;
        printer.Printf("%s\n", Sources[i]->GetName());
    }
}

}
