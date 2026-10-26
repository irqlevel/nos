#include "stack_probe.h"

namespace kernel
{

namespace stack_probe
{

void poison(void* base, ulong size)
{
    stdlib::mem_set(base, PATTERN_BYTE, size);
}

ulong untouched(const void* base, ulong size)
{
    const ulong* word = (const ulong*)base;
    ulong count = size / sizeof(ulong);

    ulong i = 0;
    while (i < count && word[i] == PATTERN)
        i++;

    return i * sizeof(ulong);
}

}

}
