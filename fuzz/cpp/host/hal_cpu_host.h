// The Hal:: CPU wrappers of hal/cpu.h, for a process on the host: the
// interrupt flag is a word the stand-ins keep (kernel.cpp), the stack and
// frame pointers are the host's, and the cycle counter counts.
#pragma once

#include <include/types.h>

namespace Fuzz
{
/* The interrupt flag, as the kernel under test last left it. */
extern bool InterruptsOn;
extern u64 Cycles;
[[noreturn]] void HostHalUnreachable(const char* what);
}

namespace Hal
{

static inline bool IsInterruptEnabled()
{
    return Fuzz::InterruptsOn;
}

/* Bit 0 the flag before; bit 63 is the kernel's to stash one of its own
   in, as on both arches. */
static inline ulong IrqSave()
{
    ulong flags = Fuzz::InterruptsOn ? 1 : 0;
    Fuzz::InterruptsOn = false;
    return flags;
}

static inline void IrqRestore(ulong flags)
{
    Fuzz::InterruptsOn = (flags & 1) != 0;
}

static inline ulong GetSp()
{
    return reinterpret_cast<ulong>(__builtin_frame_address(0));
}

static inline void SetSp(ulong newValue)
{
    (void)newValue;
    Fuzz::HostHalUnreachable("Hal::SetSp");
}

static inline ulong GetFp()
{
    return reinterpret_cast<ulong>(__builtin_frame_address(0));
}

static inline u64 ReadCycleCounter()
{
    return ++Fuzz::Cycles;
}

}
