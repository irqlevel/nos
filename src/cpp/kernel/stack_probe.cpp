#include "stack_probe.h"

namespace Kernel
{

namespace StackProbe
{

void Poison(void* base, ulong size)
{
    Stdlib::MemSet(base, PatternByte, size);
}

ulong Untouched(const void* base, ulong size)
{
    const ulong* word = (const ulong*)base;
    ulong count = size / sizeof(ulong);

    ulong i = 0;
    while (i < count && word[i] == Pattern)
        i++;

    return i * sizeof(ulong);
}

}

}
