#pragma once

#include <include/types.h>

// arm64 bodies for the Hal:: MMU wrappers (see hal/mmu.h). The kernel
// address space lives in TTBR1_EL1; TTBR0 holds a permanently empty table
// after boot. TLB flushes use the inner-shareable variants so the hardware
// broadcasts them to every CPU — no IPI shootdown needed
// (Hal::TlbShootdownNeedsIpi() returns false).

namespace Hal
{

static inline __attribute__((always_inline)) void TlbFlushPage(ulong virtAddr)
{
    asm volatile(
        "dsb ishst\n"
        "tlbi vaae1is, %0\n"
        "dsb ish\n"
        "isb\n"
        :: "r"(virtAddr >> 12) : "memory");
}

static inline __attribute__((always_inline)) void TlbFlushAll()
{
    asm volatile(
        "dsb ishst\n"
        "tlbi vmalle1is\n"
        "dsb ish\n"
        "isb\n"
        ::: "memory");
}

static inline __attribute__((always_inline)) bool TlbShootdownNeedsIpi()
{
    return false; /* tlbi *is broadcasts in hardware */
}

static inline __attribute__((always_inline)) ulong GetTranslationRoot()
{
    ulong root;
    asm volatile("mrs %0, ttbr1_el1" : "=r"(root));
    return root & ~0xFFFUL; /* strip ASID/CnP */
}

static inline __attribute__((always_inline)) void SetTranslationRoot(ulong phys)
{
    asm volatile(
        "dsb ishst\n"      /* prior PTE stores visible to the walker */
        "msr ttbr1_el1, %0\n"
        "isb\n"
        "tlbi vmalle1\n"
        "dsb ish\n"
        "isb\n"
        :: "r"(phys) : "memory");
}

/* dsb ishst after PTE stores so table walkers observe them; no-op on x86 */
static inline __attribute__((always_inline)) void PteWriteBarrier()
{
    asm volatile("dsb ishst" ::: "memory");
}

}
