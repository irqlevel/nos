#pragma once

#include "stdlib.h"
#include "trace.h"

namespace Kernel
{

namespace Core
{

class MmIo
{
public:
    static u32 Read32(void *addr)
    {
        u32 result = *reinterpret_cast<volatile u32 *>(addr);
        Trace(MmIoLL, "MmIo read 0x%p result 0x%p", addr, (ulong)result);
        return result;
    }

    static void Write32(void *addr, u32 value)
    {
        Trace(MmIoLL, "MmIo write 0x%p value 0x%p", addr, (ulong)value);
        *reinterpret_cast<volatile u32 *>(addr) = value;
    }
};

}
}