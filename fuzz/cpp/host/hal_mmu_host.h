// The Hal:: MMU wrappers of hal/mmu.h, for a process on the host. Nothing
// here translates by itself: what the kernel does to its page tables is made
// real by whatever target models the MMU (fuzz/cpp/targets/mm.cpp), at the
// points where the hardware would see it -- a TLB flush, an entry made
// valid, the root switched. The hooks do nothing unless a target defines
// them (kernel.cpp has the defaults).
#pragma once

#include <include/types.h>

namespace Fuzz
{
void HostTlbFlushPage(ulong virtAddr);
void HostTlbFlushAll();
void HostPteMadeValid();
ulong HostGetTranslationRoot();
void HostSetTranslationRoot(ulong phys);
}

namespace Hal
{

static inline void TlbFlushPage(ulong virtAddr)
{
    Fuzz::HostTlbFlushPage(virtAddr);
}

static inline void TlbFlushAll()
{
    Fuzz::HostTlbFlushAll();
}

static inline bool TlbShootdownNeedsIpi()
{
    return true;
}

static inline void PteMadeValid()
{
    Fuzz::HostPteMadeValid();
}

static inline ulong GetTranslationRoot()
{
    return Fuzz::HostGetTranslationRoot();
}

static inline void SetTranslationRoot(ulong phys)
{
    Fuzz::HostSetTranslationRoot(phys);
}

static inline void PteWriteBarrier()
{
    asm volatile("" ::: "memory");
}

}
