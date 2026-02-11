#pragma once

#include <include/types.h>

namespace Kernel
{

inline u8 MmioRead8(volatile void* addr)
{
    return *(volatile u8*)addr;
}

inline u16 MmioRead16(volatile void* addr)
{
    return *(volatile u16*)addr;
}

inline u32 MmioRead32(volatile void* addr)
{
    return *(volatile u32*)addr;
}

inline u64 MmioRead64(volatile void* addr)
{
    return *(volatile u64*)addr;
}

inline void MmioWrite8(volatile void* addr, u8 val)
{
    *(volatile u8*)addr = val;
}

inline void MmioWrite16(volatile void* addr, u16 val)
{
    *(volatile u16*)addr = val;
}

inline void MmioWrite32(volatile void* addr, u32 val)
{
    *(volatile u32*)addr = val;
}

inline void MmioWrite64(volatile void* addr, u64 val)
{
    *(volatile u64*)addr = val;
}

}
