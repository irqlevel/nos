#pragma once

#include <include/types.h>

// x86_64 bodies for the Hal:: MMU wrappers (see hal/mmu.h).

#ifdef __cplusplus
extern "C"
{
#endif

ulong get_cr3(void);
void set_cr3(ulong addr);

#ifdef __cplusplus
}
#endif

namespace hal
{

static inline __attribute__((always_inline)) void tlb_flush_page(ulong virt_addr)
{
    asm volatile("invlpg (%0)" ::"r" (virt_addr) : "memory");
}

static inline __attribute__((always_inline)) void tlb_flush_all()
{
    set_cr3(get_cr3());
}

/* A leaf entry that was not present has just been made present, and this
   CPU is about to go through it. x86 caches no translation from a
   not-present entry, so there is nothing to invalidate, and its walk sees
   the CPU's own store as a load would: what is left is keeping the compiler
   from moving the store past the access. */
static inline __attribute__((always_inline)) void pte_made_valid()
{
    asm volatile("" ::: "memory");
}

static inline __attribute__((always_inline)) bool tlb_shootdown_needs_ipi()
{
    return true; /* invlpg/CR3 are CPU-local; remote CPUs need an IPI */
}

static inline __attribute__((always_inline)) ulong get_translation_root()
{
    return get_cr3();
}

static inline __attribute__((always_inline)) void set_translation_root(ulong phys)
{
    set_cr3(phys);
}

}
