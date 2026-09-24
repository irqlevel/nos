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

/* A leaf entry that was not present has just been made present, and this
   CPU is about to go through it. x86 caches no translation from a
   not-present entry, so there is nothing to invalidate, and its walk sees
   the CPU's own store as a load would: what is left is keeping the compiler
   from moving the store past the access. */
static inline __attribute__((always_inline)) void PteMadeValid()
{
    asm volatile("" ::: "memory");
}

static inline __attribute__((always_inline)) bool TlbShootdownNeedsIpi()
{
    return true; /* invlpg/CR3 are CPU-local; remote CPUs need an IPI */
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
