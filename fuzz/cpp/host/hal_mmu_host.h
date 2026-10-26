// The hal:: MMU wrappers of hal/mmu.h, for a process on the host. Nothing
// here translates by itself: what the kernel does to its page tables is made
// real by whatever target models the MMU (fuzz/cpp/targets/mm.cpp), at the
// points where the hardware would see it -- a TLB flush, an entry made
// valid, the root switched. The hooks do nothing unless a target defines
// them (kernel.cpp has the defaults).
#pragma once

#include <include/types.h>

namespace fuzz
{
void host_tlb_flush_page(ulong virt_addr);
void host_tlb_flush_all();
void host_pte_made_valid();
ulong host_get_translation_root();
void host_set_translation_root(ulong phys);
}

namespace hal
{

static inline void tlb_flush_page(ulong virt_addr)
{
    fuzz::host_tlb_flush_page(virt_addr);
}

static inline void tlb_flush_all()
{
    fuzz::host_tlb_flush_all();
}

static inline bool tlb_shootdown_needs_ipi()
{
    return true;
}

static inline void pte_made_valid()
{
    fuzz::host_pte_made_valid();
}

static inline ulong get_translation_root()
{
    return fuzz::host_get_translation_root();
}

static inline void set_translation_root(ulong phys)
{
    fuzz::host_set_translation_root(phys);
}

static inline void pte_write_barrier()
{
    asm volatile("" ::: "memory");
}

}
