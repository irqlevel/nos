#pragma once

#include <include/types.h>

// x86_64 bodies for the Hal:: MMU wrappers (see hal/mmu.h).

#ifdef __cplusplus
extern "C"
{
#endif

ulong GetCr3(void);
void SetCr3(ulong addr);

#ifdef __cplusplus
}
#endif

namespace Hal
{

static inline __attribute__((always_inline)) void TlbFlushPage(ulong virtAddr)
{
    asm volatile("invlpg (%0)" ::"r" (virtAddr) : "memory");
}

static inline __attribute__((always_inline)) void TlbFlushAll()
{
    SetCr3(GetCr3());
}

static inline __attribute__((always_inline)) ulong GetTranslationRoot()
{
    return GetCr3();
}

static inline __attribute__((always_inline)) void SetTranslationRoot(ulong phys)
{
    SetCr3(phys);
}

}
