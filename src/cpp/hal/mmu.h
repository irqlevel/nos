#pragma once

// MMU control: TLB invalidation and the translation-root register.
// Provides namespace Hal { TlbFlushPage, TlbFlushAll, GetTranslationRoot,
// SetTranslationRoot }. Local-CPU semantics only; cross-CPU shootdown is
// built on top via IPIs (kernel/cpu.h InvalidateTlb*).
#if defined(__x86_64__)
#include <arch/x86_64/hal_mmu_inline.h>
#else
#error "unsupported architecture"
#endif
