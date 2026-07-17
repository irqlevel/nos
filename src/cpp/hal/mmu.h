#pragma once

#include <include/types.h>

namespace Hal
{
/* Enable hardware support required for W^X. x86: set EFER.NXE so the NX
   PTE bit is honored (must run before any PTE has bit 63 set, i.e. before
   loading a CR3 whose table carries NX bits). arm64: no-op (PXN/UXN are
   always active). Defined per arch. */
void EnableWxSupport();

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
