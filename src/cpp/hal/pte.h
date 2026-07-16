#pragma once

// Kernel::Mm::Pte / PtePage: the arch-specific page-table entry encoding.
// The PageTable public API (mm/page_table.h) is the arch-neutral MMU
// surface; Pte's accessor/helper names are the cross-arch contract.
#if defined(__x86_64__)
#include <arch/x86_64/pte.h>
#else
#error "unsupported architecture"
#endif
