#pragma once

#include <include/types.h>

namespace kernel
{

inline u8 mmio_read8(volatile void* addr)
{
    return *(volatile u8*)addr;
}

inline u16 mmio_read16(volatile void* addr)
{
    return *(volatile u16*)addr;
}

inline u32 mmio_read32(volatile void* addr)
{
    return *(volatile u32*)addr;
}

inline u64 mmio_read64(volatile void* addr)
{
    return *(volatile u64*)addr;
}

inline void mmio_write8(volatile void* addr, u8 val)
{
    *(volatile u8*)addr = val;
}

inline void mmio_write16(volatile void* addr, u16 val)
{
    *(volatile u16*)addr = val;
}

inline void mmio_write32(volatile void* addr, u32 val)
{
    *(volatile u32*)addr = val;
}

inline void mmio_write64(volatile void* addr, u64 val)
{
    *(volatile u64*)addr = val;
}

}
