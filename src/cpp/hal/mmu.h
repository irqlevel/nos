#pragma once

#include <include/types.h>

namespace hal
{
/* Enable hardware support required for W^X. x86: set EFER.NXE so the NX
   PTE bit is honored (must run before any PTE has bit 63 set, i.e. before
   loading a CR3 whose table carries NX bits). arm64: no-op (PXN/UXN are
   always active). Defined per arch. */
void enable_wx_support();

/* Program this CPU's memory-type table so write-combining MMIO mappings
   work. x86: IA32_PAT entry 4 (the entry a 4K PTE selects with the PAT
   bit) becomes WC, leaving the other seven entries at their power-on
   meaning so existing mappings keep their type. The architecture requires
   every CPU to run with the same PAT, so this must be called on the BSP
   and on each AP before it loads the kernel page table. arm64: no-op, the
   MAIR (idx2 = Normal non-cacheable) is set up in boot.S for every CPU.
   Defined per arch. */
void setup_memory_types();

/* True once setup_memory_types() has made write-combining usable (x86: the
   CPU has PAT). map_mmio_region falls back to uncached when it is false. */
bool is_write_combining_available();

/* Nonzero if the arch guarantees [phys_addr, phys_addr+size) is permanently
   mapped (arm64: the boot device-GiB block covering all QEMU-virt MMIO);
   map_mmio_region returns it directly instead of building 4K mappings.
   x86 returns 0. Defined per arch. */
ulong mmio_premapped_va(ulong phys_addr, ulong size_bytes);

/* Make code just stored to [va, va+size) safe to execute: the stores reach
   the point of unification, and no stale instruction stays cached for the
   range. arm64 does both by hand (DC CVAU, IC IVAU); x86 keeps instruction
   fetch coherent with stores and needs nothing. The module loader calls it
   before a module's first instruction runs. Defined per arch. */
void sync_instruction_cache(ulong va, ulong size);
}

// MMU control: TLB invalidation and the translation-root register.
// Provides namespace Hal { tlb_flush_page, tlb_flush_all, pte_made_valid,
// get_translation_root, set_translation_root }. Local-CPU semantics only;
// cross-CPU shootdown is built on top via IPIs (kernel/cpu.h InvalidateTlb*).
#if defined(__x86_64__)
#include <arch/x86_64/hal_mmu_inline.h>
#elif defined(__aarch64__)
#include <arch/arm64/hal_mmu_inline.h>
#else
#error "unsupported architecture"
#endif
