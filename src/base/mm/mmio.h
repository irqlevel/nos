#pragma once

#include <kernel/trace.h>
#include <lib/stdlib.h>

namespace kernel
{

namespace mm
{

class MmIo
{
public:
    static u32 read32(void *addr)
    {
        u32 result = *reinterpret_cast<volatile u32 *>(addr);
        trace(MM_IO_LL, "MmIo read 0x%p result 0x%lX", addr, (ulong)result);
        return result;
    }

    static void write32(void *addr, u32 value)
    {
        trace(MM_IO_LL, "MmIo write 0x%p value 0x%lX", addr, (ulong)value);
        *reinterpret_cast<volatile u32 *>(addr) = value;
    }
};

}
}