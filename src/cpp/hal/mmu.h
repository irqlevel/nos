#pragma once

#include <include/types.h>

namespace Hal
{
/* Enable hardware support required for W^X. x86: set EFER.NXE so the NX
   PTE bit is honored (must run before any PTE has bit 63 set, i.e. before
   loading a CR3 whose table carries NX bits). arm64: no-op (PXN/UXN are
   always active). Defined per arch. */
void EnableWxSupport();

/* Program this CPU's memory-type table so write-combining MMIO mappings
   work. x86: IA32_PAT entry 4 (the entry a 4K PTE selects with the PAT
   bit) becomes WC, leaving the other seven entries at their power-on
   meaning so existing mappings keep their type. The architecture requires
   every CPU to run with the same PAT, so this must be called on the BSP
   and on each AP before it loads the kernel page table. arm64: no-op, the
   MAIR (idx2 = Normal non-cacheable) is set up in boot.S for every CPU.
   Defined per arch. */
void SetupMemoryTypes();

/* True once SetupMemoryTypes() has made write-combining usable (x86: the
   CPU has PAT). MapMmioRegion falls back to uncached when it is false. */
bool IsWriteCombiningAvailable();

/* Nonzero if the arch guarantees [physAddr, physAddr+size) is permanently
   mapped (arm64: the boot device-GiB block covering all QEMU-virt MMIO);
   MapMmioRegion returns it directly instead of building 4K mappings.
   x86 returns 0. Defined per arch. */
ulong MmioPremappedVa(ulong physAddr, ulong sizeBytes);
}

// MMU control: TLB invalidation and the translation-root register.
// Provides namespace Hal { TlbFlushPage, TlbFlushAll, GetTranslationRoot,
// SetTranslationRoot }. Local-CPU semantics only; cross-CPU shootdown is
// built on top via IPIs (kernel/cpu.h InvalidateTlb*).
#if defined(__x86_64__)
#include <arch/x86_64/hal_mmu_inline.h>
#elif defined(__aarch64__)
#include <arch/arm64/hal_mmu_inline.h>
#else
#error "unsupported architecture"
#endif
